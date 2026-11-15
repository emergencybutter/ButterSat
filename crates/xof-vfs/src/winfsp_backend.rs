//! WinFsp backend (Windows).

use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    STATUS_END_OF_FILE, STATUS_INVALID_DEVICE_REQUEST, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNEXPECTED_IO_ERROR,
};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::U16CStr;
use xof_core::engine::ReaderClass;

use crate::process::ProcessClassifier;
use crate::scenery::{Attr, Node, Scenery};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x2000;

/// Make sure the system WinFsp DLL can be loaded, then initialise.
pub fn init() -> anyhow::Result<()> {
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    unsafe {
        let mut buf = [0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        let key: Vec<u16> = "SOFTWARE\\WOW6432Node\\WinFsp\0".encode_utf16().collect();
        let val: Vec<u16> = "InstallDir\0".encode_utf16().collect();
        let r = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        );
        if r.is_ok() {
            let dir = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
            let dll = format!("{}\\bin\\winfsp-x64.dll\0", dir.trim_end_matches('\\'));
            let w: Vec<u16> = dll.encode_utf16().collect();
            let _ = LoadLibraryW(PCWSTR(w.as_ptr()));
        }
    }
    winfsp::winfsp_init().map_err(|e| anyhow::anyhow!("WinFsp is not installed or could not be loaded ({e:?})"))?;
    Ok(())
}

/// Is the WinFsp driver installed?
pub fn available() -> bool {
    init().is_ok()
}

fn filetime(t: SystemTime) -> u64 {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() / 100).unwrap_or(0) as u64;
    secs + 116_444_736_000_000_000
}

fn index_number(path: &str) -> u64 {
    // FNV-1a over the lowercase path.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_ascii_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn fill(info: &mut FileInfo, attr: &Attr, path: &str) {
    info.file_attributes = if attr.is_dir {
        FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED
    } else {
        FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED
    };
    info.reparse_tag = 0;
    info.file_size = attr.size;
    info.allocation_size = attr.size.div_ceil(4096) * 4096;
    let t = filetime(attr.mtime);
    info.creation_time = t;
    info.last_access_time = t;
    info.last_write_time = t;
    info.change_time = t;
    info.index_number = index_number(path);
    info.hard_links = 0;
    info.ea_size = 0;
}

/// A security descriptor granting everyone read/execute, owner/admins full.
struct Security(Vec<u8>);

impl Security {
    fn new() -> anyhow::Result<Self> {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::PSECURITY_DESCRIPTOR;
        let sddl: Vec<u16> = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;WD)\0".encode_utf16().collect();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let mut len = 0u32;
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                Some(&mut len),
            )?;
            let bytes = std::slice::from_raw_parts(psd.0 as *const u8, len as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(psd.0)));
            Ok(Self(bytes))
        }
    }

    fn copy_to(&self, out: Option<&mut [c_void]>) -> u64 {
        if let Some(out) = out {
            if out.len() >= self.0.len() {
                unsafe { std::ptr::copy_nonoverlapping(self.0.as_ptr(), out.as_mut_ptr().cast::<u8>(), self.0.len()) };
            }
        }
        self.0.len() as u64
    }
}

pub struct Handle {
    node: Node,
    path: String,
    class: ReaderClass,
    dir: DirBuffer,
}

pub struct WinFs {
    scenery: Arc<Scenery>,
    classifier: ProcessClassifier,
    security: Security,
}

impl WinFs {
    fn path_of(name: &U16CStr) -> String {
        name.to_string_lossy()
    }

    fn requester_class(&self) -> ReaderClass {
        let pid = unsafe { winfsp_sys::FspFileSystemOperationProcessIdF() };
        self.classifier.classify(pid)
    }

    fn not_found() -> winfsp::FspError {
        STATUS_OBJECT_NAME_NOT_FOUND.into()
    }
}

impl FileSystemContext for WinFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = Self::path_of(file_name);
        let node = self.scenery.resolve(&path).ok_or_else(Self::not_found)?;
        let attributes = if node.is_dir() { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_READONLY };
        let sz = self.security.copy_to(security_descriptor);
        Ok(FileSecurity { reparse: false, sz_security_descriptor: sz, attributes })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = Self::path_of(file_name);
        let node = self.scenery.resolve(&path).ok_or_else(Self::not_found)?;
        let attr = self.scenery.attr(&node).ok_or_else(Self::not_found)?;
        fill(file_info.as_mut(), &attr, &path);
        let class = self.requester_class();
        if !node.is_dir() {
            self.scenery.on_open(&node, class);
            if !self.scenery.cacheable(&node) {
                unsafe {
                    self.with_operation_response(|rsp| rsp.Rsp.Create.Opened.set_DisableCache(1));
                }
            }
        }
        Ok(Handle { node, path, class, dir: DirBuffer::new() })
    }

    fn close(&self, _context: Self::FileContext) {}

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let attr = self.scenery.attr(&context.node).ok_or_else(Self::not_found)?;
        fill(file_info, &attr, &context.path);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        Ok(self.security.copy_to(security_descriptor))
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        let attr = self.scenery.attr(&context.node).ok_or_else(Self::not_found)?;
        if offset >= attr.size {
            return Err(STATUS_END_OF_FILE.into());
        }
        match self.scenery.read_blocking(&context.node, offset, buffer.len() as u64, context.class) {
            Ok(data) => {
                buffer[..data.len()].copy_from_slice(&data);
                Ok(data.len() as u32)
            }
            Err(e) => {
                tracing::warn!("read {} @{offset}: {e}", context.path);
                Err(STATUS_UNEXPECTED_IO_ERROR.into())
            }
        }
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let Node::Dir(dir) = &context.node else {
            return Err(STATUS_INVALID_DEVICE_REQUEST.into());
        };
        if let Ok(lock) = context.dir.acquire(marker.is_none(), None) {
            let dir_attr = Attr { is_dir: true, size: 0, mtime: SystemTime::now() };
            if !dir.is_empty() {
                for dots in [".", ".."] {
                    let mut di: DirInfo = DirInfo::new();
                    fill(di.file_info_mut(), &dir_attr, dir);
                    di.set_name(dots)?;
                    lock.write(&mut di)?;
                }
            }
            for (name, node) in self.scenery.list(dir) {
                let Some(attr) = self.scenery.attr(&node) else { continue };
                let mut di: DirInfo = DirInfo::new();
                let child = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
                fill(di.file_info_mut(), &attr, &child);
                if di.set_name(&name).is_err() {
                    continue;
                }
                lock.write(&mut di)?;
            }
        }
        Ok(context.dir.read(marker, buffer))
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        out.total_size = 1 << 40;
        out.free_size = 0;
        out.set_volume_label("xporthofs");
        Ok(())
    }
}

/// A mounted WinFsp filesystem; unmounts on drop.
pub struct Mount {
    host: FileSystemHost<WinFs, winfsp::host::FineGuard>,
}

impl Mount {
    /// Mount `scenery` at `mount_point` (a directory path that must not exist;
    /// its parent must).
    pub fn new(scenery: Arc<Scenery>, mount_point: &Path) -> anyhow::Result<Self> {
        init()?;
        let mut vp = VolumeParams::new();
        vp.sector_size(4096)
            .sectors_per_allocation_unit(1)
            .max_component_length(255)
            .volume_creation_time(filetime(SystemTime::now()))
            .volume_serial_number(0x0f0f_2026)
            .file_info_timeout(2000)
            .case_sensitive_search(false)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .persistent_acls(true)
            .read_only_volume(true)
            .post_cleanup_when_modified_only(true)
            .filesystem_name("xporthofs");
        let classifier = ProcessClassifier::new(scenery.xplane_processes());
        let fs = WinFs { scenery, classifier, security: Security::new()? };
        let mut host: FileSystemHost<WinFs, winfsp::host::FineGuard> =
            FileSystemHost::new(vp, fs).map_err(|e| anyhow::anyhow!("WinFsp host: {e:?}"))?;
        host.mount(mount_point.as_os_str()).map_err(|e| anyhow::anyhow!("mount {}: {e:?}", mount_point.display()))?;
        let threads = std::thread::available_parallelism().map(|n| n.get() as u32 * 2).unwrap_or(16).max(16);
        host.start_with_threads(threads).map_err(|e| anyhow::anyhow!("WinFsp dispatcher: {e:?}"))?;
        Ok(Self { host })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.host.stop();
        self.host.unmount();
    }
}
