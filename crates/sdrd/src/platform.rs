//! The platform layer the no-OS driver calls back into: GPIO through the
//! kernel's GPIO character device, FPGA register access through UIO.
//! (SPI and delays come from ADI's own linux-userspace platform, in C.)

use crate::noos::*;
use std::{
    ffi::c_void,
    fs,
    os::fd::AsRawFd,
    sync::Mutex,
};

// ------------------------------------------------------------------ AXI ----

struct Window {
    phys: u32,
    len: u32,
    ptr: *mut u32,
}
// The mapping is process-global MMIO; access is serialised by the mutex.
unsafe impl Send for Window {}

static WINDOWS: Mutex<Vec<Window>> = Mutex::new(Vec::new());

/// Map every UIO region once.  The driver addresses cores by physical address,
/// so a register access is a lookup by range, not a /dev/uioN number that
/// depends on probe order.
fn windows() -> std::sync::MutexGuard<'static, Vec<Window>> {
    let mut w = WINDOWS.lock().unwrap();
    if w.is_empty() {
        for n in 0.. {
            let base = format!("/sys/class/uio/uio{n}/maps/map0");
            let hex = |f: &str| {
                fs::read_to_string(format!("{base}/{f}"))
                    .ok()
                    .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            };
            let (Some(phys), Some(len)) = (hex("addr"), hex("size")) else { break };
            let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(format!("/dev/uio{n}")) else { continue };
            // SAFETY: mapping a UIO region the kernel exported for exactly this.
            let ptr = unsafe {
                libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ | libc::PROT_WRITE,
                           libc::MAP_SHARED, f.as_raw_fd(), 0)
            };
            if ptr != libc::MAP_FAILED {
                w.push(Window { phys, len, ptr: ptr.cast() });
            }
        }
    }
    w
}

fn reg(addr: u32) -> Option<*mut u32> {
    windows()
        .iter()
        .find(|w| addr >= w.phys && addr - w.phys + 4 <= w.len)
        // SAFETY: bounds checked against the window above.
        .map(|w| unsafe { w.ptr.byte_add((addr - w.phys) as usize) })
}

pub fn reg_read(addr: u32) -> Option<u32> {
    // SAFETY: `reg` only returns pointers inside a live MMIO mapping.
    reg(addr).map(|p| unsafe { p.read_volatile() })
}

pub fn reg_write(addr: u32, val: u32) -> bool {
    // SAFETY: as above.
    reg(addr).map(|p| unsafe { p.write_volatile(val) }).is_some()
}

#[no_mangle]
pub extern "C" fn no_os_axi_io_read(base: u32, offset: u32, data: *mut u32) -> i32 {
    match reg_read(base.wrapping_add(offset)) {
        // SAFETY: the driver passes a valid out-pointer.
        Some(v) => unsafe { *data = v; 0 },
        None => -libc::EFAULT,
    }
}

#[no_mangle]
pub extern "C" fn no_os_axi_io_write(base: u32, offset: u32, data: u32) -> i32 {
    if reg_write(base.wrapping_add(offset), data) { 0 } else { -libc::EFAULT }
}

// ----------------------------------------------------------------- GPIO ----
// One requested line per descriptor; the line's fd lives in `extra`.

const fn iowr(nr: u32, size: usize) -> libc::c_ulong {
    ((3u32 << 30) | ((size as u32) << 16) | (0xB4 << 8) | nr) as libc::c_ulong
}
const GET_LINE: libc::c_ulong = iowr(0x07, size_of::<gpio_v2_line_request>());
const SET_CONFIG: libc::c_ulong = iowr(0x0D, size_of::<gpio_v2_line_config>());
const GET_VALUES: libc::c_ulong = iowr(0x0E, size_of::<gpio_v2_line_values>());
const SET_VALUES: libc::c_ulong = iowr(0x0F, size_of::<gpio_v2_line_values>());
const FLAG_INPUT: u64 = 1 << 2;
const FLAG_OUTPUT: u64 = 1 << 3;

fn line_fd(d: *mut no_os_gpio_desc) -> i32 {
    // SAFETY: descriptors are only ever created by `gpio_get` below.
    unsafe { (*d).extra as isize as i32 }
}

unsafe extern "C" fn gpio_get(desc: *mut *mut no_os_gpio_desc, param: *const no_os_gpio_init_param) -> i32 {
    let Ok(chip) = fs::File::open("/dev/gpiochip0") else { return -libc::ENODEV };
    let mut req = gpio_v2_line_request::default();
    req.offsets[0] = (*param).number as u32;
    req.num_lines = 1;
    req.config.flags = FLAG_INPUT;
    for (d, s) in req.consumer.iter_mut().zip(b"sdrd") {
        *d = *s as _;
    }
    if libc::ioctl(chip.as_raw_fd(), GET_LINE as _, &mut req) < 0 {
        return -libc::EBUSY;
    }
    let d = libc::calloc(1, size_of::<no_os_gpio_desc>()) as *mut no_os_gpio_desc;
    if d.is_null() {
        libc::close(req.fd);
        return -libc::ENOMEM;
    }
    (*d).number = (*param).number;
    (*d).platform_ops = (*param).platform_ops;
    (*d).extra = req.fd as isize as *mut c_void;
    *desc = d;
    0
}

unsafe extern "C" fn gpio_remove(d: *mut no_os_gpio_desc) -> i32 {
    if !d.is_null() {
        libc::close(line_fd(d));
        libc::free(d.cast());
    }
    0
}

unsafe fn set_flags(d: *mut no_os_gpio_desc, flags: u64, value: Option<u8>) -> i32 {
    let mut cfg = gpio_v2_line_config::default();
    cfg.flags = flags;
    if let Some(v) = value {
        cfg.num_attrs = 1;
        cfg.attrs[0].attr.id = gpio_v2_line_attr_id_GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES;
        cfg.attrs[0].attr.__bindgen_anon_1.values = v as u64;
        cfg.attrs[0].mask = 1;
    }
    if libc::ioctl(line_fd(d), SET_CONFIG as _, &mut cfg) < 0 { -libc::EIO } else { 0 }
}

unsafe extern "C" fn gpio_input(d: *mut no_os_gpio_desc) -> i32 {
    set_flags(d, FLAG_INPUT, None)
}

unsafe extern "C" fn gpio_output(d: *mut no_os_gpio_desc, v: u8) -> i32 {
    set_flags(d, FLAG_OUTPUT, Some(v))
}

unsafe extern "C" fn gpio_get_direction(_d: *mut no_os_gpio_desc, dir: *mut u8) -> i32 {
    *dir = NO_OS_GPIO_OUT as u8;
    0
}

unsafe extern "C" fn gpio_set_value(d: *mut no_os_gpio_desc, v: u8) -> i32 {
    let mut vals = gpio_v2_line_values { bits: v as u64, mask: 1 };
    if libc::ioctl(line_fd(d), SET_VALUES as _, &mut vals) < 0 { -libc::EIO } else { 0 }
}

unsafe extern "C" fn gpio_get_value(d: *mut no_os_gpio_desc, v: *mut u8) -> i32 {
    let mut vals = gpio_v2_line_values { bits: 0, mask: 1 };
    if libc::ioctl(line_fd(d), GET_VALUES as _, &mut vals) < 0 {
        return -libc::EIO;
    }
    *v = (vals.bits & 1) as u8;
    0
}

#[no_mangle]
pub static pmp_gpio_ops: no_os_gpio_platform_ops = no_os_gpio_platform_ops {
    gpio_ops_get: Some(gpio_get),
    gpio_ops_get_optional: Some(gpio_get),
    gpio_ops_remove: Some(gpio_remove),
    gpio_ops_direction_input: Some(gpio_input),
    gpio_ops_direction_output: Some(gpio_output),
    gpio_ops_get_direction: Some(gpio_get_direction),
    gpio_ops_set_value: Some(gpio_set_value),
    gpio_ops_get_value: Some(gpio_get_value),
};
