use std::io::Write;
use std::mem::ManuallyDrop;
use std::time::Instant;
use libc::{backtrace, c_void};

use crate::trace::{Trace, TraceTree};

pub struct HeaptrackWriter<W> {
    inner: LineWriter<W>,
    start_time: Instant,
    trace_tree: TraceTree,
    module_cache_dirty: bool,
}

impl<W: Write> HeaptrackWriter<W> {
    const VERSION: usize = 0x10350;
    const FILE_FORMAT_VERSION: usize = 3;

    pub fn new(inner: W, start_time: Instant) -> Self {
        Self {
            inner: LineWriter { writer: inner.into() },
            start_time,
            trace_tree: Default::default(),
            module_cache_dirty: true,
        }
    }

    pub fn init(&mut self) {
        self.write_version();
        self.write_exe();
        self.write_command_line();
        self.write_system_info();
        self.write_suppressions();
    }

    fn write_version(&mut self) {
        self.inner.write_hex_line('v', &[Self::VERSION, Self::FILE_FORMAT_VERSION]).expect("write failed")
    }

    fn write_exe(&mut self) {
        let buf = std::fs::read_link("/proc/self/exe").expect("read_link failed");
        let exe = buf.to_string_lossy();
        write!(self.inner.writer, "x {:x} {}\0\n", exe.len(), exe).expect("write failed");
    }

    fn write_command_line(&mut self) {
        let cmd_line = std::fs::read("/proc/self/cmdline").expect("read failed");
        let mut slice = &cmd_line[..];
        write!(self.inner.writer, "X").expect("write failed");
        while slice.len() > 1 {
            let end = slice.iter().position(|x| *x == 0).unwrap_or_else(|| slice.len());
            write!(self.inner.writer, " ").expect("write failed");
            self.inner.writer.write_all(&slice[..end]).expect("write_all failed");
            slice = &slice[end..];
        }
        write!(self.inner.writer, "\n").expect("write failed");
    }

    fn write_system_info(&mut self) {
        // I _SC_PAGESIZE _SC_PHYS_PAGES
        self.inner.write_hex_line('I', &[4096, 1]).expect("write failed");
    }

    fn write_suppressions(&mut self) {
        // Not yet implemented
    }

    fn write_timestamp(&mut self) {
        self.inner.write_hex_line('c', &[self.start_time.elapsed().as_millis() as usize]).expect("write failed")
    }

    pub fn handle_malloc(&mut self, ptr: *mut u8, size: usize, trace: &Trace) {
        self.update_module_cache();
        let inner = &mut self.inner;
        let index = self.trace_tree.index(trace, move |ip, index| {
            inner.write_hex_line('t', &[ip as usize - 1, index as _]).expect("write failed");
            true
        });
        self.inner.write_hex_line('+', &[size as _, index as _, ptr as _]).expect("write failed");
    }

    pub fn handle_free(&mut self, ptr: *mut u8) {
        self.inner.write_hex_line('-', &[ptr as _]).expect("write failed");
    }

    fn update_module_cache(&mut self) {
        if self.module_cache_dirty {
            self.module_cache_dirty = false;
            self.inner.writer.write_all(b"m 1 -\n").expect("write_all failed");

            unsafe extern "C" fn dl_iterate_phdr_callback(info: *mut libc::dl_phdr_info, _size: libc::size_t, data: *mut c_void) -> libc::c_int {
                let writer: Box<Box<&mut dyn Write>> = Box::from_raw(data as _);
                let mut writer = ManuallyDrop::new(writer);

                let filename = if (*info).dlpi_name.is_null() {
                    "x".to_owned()
                } else {
                    let cstr = std::ffi::CStr::from_ptr((*info).dlpi_name);
                    let bytes = cstr.to_bytes();
                    if bytes.len() == 0 {
                        "x".to_owned()
                    } else {
                        String::from(cstr.to_string_lossy())
                    }
                };

                write!(writer, "m {:x} {} {:x}", filename.len(), filename, (*info).dlpi_addr as u64).expect("write failed");
                let phdrs = std::slice::from_raw_parts((*info).dlpi_phdr, (*info).dlpi_phnum as _);
                for phdr in phdrs {
                    if phdr.p_type == libc::PT_LOAD {
                        write!(writer, " {:x} {:x}", phdr.p_vaddr, phdr.p_memsz).expect("write failed");
                    }
                }
                write!(writer, "\n").expect("write failed");

                0
            }

            let writer = Box::new(Box::new(&mut self.inner.writer as &mut dyn Write));
            let data = Box::into_raw(writer);
            unsafe {
                libc::dl_iterate_phdr(Some(dl_iterate_phdr_callback), data as _);
                let _ = Box::from_raw(data);
            }
        }
    }
}

struct LineWriter<W> {
    writer: Box<W>,
}

impl<W: Write> LineWriter<W> {
    fn write_hex_line(&mut self, type_char: char, numbers: &[usize]) -> std::io::Result<()> {
        write!(self.writer, "{}", type_char)?;
        for number in numbers {
                write!(self.writer, " {:x}", number)?;
        }
        write!(self.writer, "\n")?;
        Ok(())
    }
}
