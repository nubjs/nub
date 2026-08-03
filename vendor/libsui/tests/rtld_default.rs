extern "C" {
    fn dlsym(handle: isize, symbol: *const u8) -> usize;
    fn dlerror() -> *const u8;
}

#[no_mangle]
pub extern "C" fn exported_sym() {
    println!("Hello from exported_sym");
}

const RTLD_DEFAULT: isize = -2;

fn main() {
    if unsafe { dlsym(RTLD_DEFAULT, "exported_sym\0".as_ptr()) } == 0 {
        let err = unsafe { dlerror() };
        let err = unsafe { std::ffi::CStr::from_ptr(err as _) };
        let err = err.to_str().unwrap();
        eprintln!("Error: {}", err);
        panic!("Symbol not found");
    }
}
