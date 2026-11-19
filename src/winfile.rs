//! Rå Win32-filåtkomst: mmap av källfil, write-through-filer, atomisk sparning.

use std::io::{self, Read};
use std::time::UNIX_EPOCH;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Memory::*;
use windows::core::PCWSTR;

use crate::orig::{self, Orig, Store};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn werr(e: windows::core::Error) -> io::Error {
    io::Error::other(format!("{e}"))
}

// ---------------------------------------------------------------- källdata

/// Read-only vy av en fil. Öppnas utan FILE_SHARE_WRITE: ingen annan process
/// kan skriva i filen medan den är mappad, så &[u8] över vyn är sund.
pub struct Mapping {
    file: HANDLE,
    map: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

// Vyn är read-only och handtagen är processglobala; säkert att dela mellan trådar
// (indextråd och sökning läser samtidigt som UI-tråden).
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.map);
            let _ = CloseHandle(self.file);
        }
    }
}

/// Öppnar basfil + identitet (storlek, mtime). Körs på loader-tråden.
/// Är filen låst för skrivning av annan process läses en ögonblicksbild
/// av den storlek filen hade vid öppning (växande loggar hänger inte).
pub fn open_orig(path: &str) -> io::Result<(Orig, (u64, u64))> {
    match map_file(path) {
        Ok(o) => Ok((o, file_ident(path)?)),
        Err(_) => {
            let (len, mtime) = file_ident(path)?;
            let f = std::fs::File::open(path)?;
            let mut v = Vec::with_capacity(len as usize);
            f.take(len).read_to_end(&mut v)?;
            let n = v.len() as u64;
            Ok((orig::owned(v), (n, mtime)))
        }
    }
}

fn map_file(path: &str) -> io::Result<Orig> {
    let w = wide(path);
    unsafe {
        let file = CreateFileW(
            PCWSTR(w.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(werr)?;
        let mut size = 0i64;
        if let Err(e) = GetFileSizeEx(file, &mut size) {
            let _ = CloseHandle(file);
            return Err(werr(e));
        }
        if size == 0 {
            let _ = CloseHandle(file);
            return Ok(orig::empty());
        }
        let map = match CreateFileMappingW(file, None, PAGE_READONLY, 0, 0, PCWSTR::null()) {
            Ok(m) => m,
            Err(e) => {
                let _ = CloseHandle(file);
                return Err(werr(e));
            }
        };
        let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
        if view.Value.is_null() {
            let _ = CloseHandle(map);
            let _ = CloseHandle(file);
            return Err(io::Error::last_os_error());
        }
        Ok(orig::make(Store::Mapped(Mapping {
            file,
            map,
            view,
            len: size as usize,
        })))
    }
}

/// (storlek, mtime ns) – identifierar basfilen som en WAL är relativ till.
pub fn file_ident(path: &str) -> io::Result<(u64, u64)> {
    let m = std::fs::metadata(path)?;
    let t = m
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Ok((m.len(), t))
}

// ---------------------------------------------------------------- skrivning

/// Skrivfil med FILE_FLAG_WRITE_THROUGH: varje WriteFile når mediet innan retur.
pub struct WinFile(HANDLE);

impl WinFile {
    pub fn create(path: &str, truncate: bool) -> io::Result<WinFile> {
        let w = wide(path);
        unsafe {
            let h = CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_READ,
                None,
                if truncate { CREATE_ALWAYS } else { OPEN_ALWAYS },
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH,
                None,
            )
            .map_err(werr)?;
            let f = WinFile(h);
            if !truncate {
                SetFilePointerEx(h, 0, None, FILE_END).map_err(werr)?;
            }
            Ok(f)
        }
    }

    pub fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let chunk = &buf[..buf.len().min(1 << 30)];
            let mut n = 0u32;
            unsafe { WriteFile(self.0, Some(chunk), Some(&mut n), None) }.map_err(werr)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[n as usize..];
        }
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        unsafe { FlushFileBuffers(self.0) }.map_err(werr)
    }
}

impl Drop for WinFile {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// tmp + ReplaceFileW (behåller ACL/attribut). Fallback: rename, sist direktskrivning.
pub fn write_file_atomic(path: &str, data: &[u8]) -> io::Result<()> {
    let tmp = format!("{path}.pytte-tmp");
    {
        let mut f = WinFile::create(&tmp, true)?;
        f.write_all(data)?;
        f.sync()?;
    }
    let (wp, wt) = (wide(path), wide(&tmp));
    let replaced = unsafe {
        ReplaceFileW(
            PCWSTR(wp.as_ptr()),
            PCWSTR(wt.as_ptr()),
            PCWSTR::null(),
            REPLACEFILE_WRITE_THROUGH,
            None,
            None,
        )
    };
    if replaced.is_ok() {
        return Ok(());
    }
    if std::fs::rename(&tmp, path).is_ok() {
        return Ok(());
    }
    let mut f = WinFile::create(path, true)?;
    f.write_all(data)?;
    f.sync()?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}
