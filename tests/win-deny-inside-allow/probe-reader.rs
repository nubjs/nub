// A CLR-FREE file reader for the Q1 AppContainer cells.
//
// Why this exists: the C# probe-child hosts the .NET Framework CLR, and an LPAC process cannot load
// it -- the Framework directories grant ALL APPLICATION PACKAGES, which LPAC ignores by definition,
// so every LPAC cell died at 0x80131702 CLR_E_SHIM_RUNTIMELOAD before the child ever ran. Regranting
// those directories is not possible either: they are TrustedInstaller-owned and icacls is denied
// even to an elevated admin. A native binary sidesteps the whole problem -- it needs only the MSVC
// CRT in System32, which already carries an ARAP grant.
//
// It is also the independent second reader for the ordinary (non-LPAC) cells, so the headline
// result cannot be an artifact of one runtime's file-open path.
//
// Exit contract, identical to probe-child.exe: 0 = read OK, 5 = ACCESS_DENIED, 9 = other error.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: probe-reader <path>");
        std::process::exit(2);
    }
    match std::fs::read(&args[1]) {
        Ok(bytes) => {
            println!("READER ok len={}", bytes.len());
            std::process::exit(0);
        }
        Err(e) => {
            // raw_os_error is the load-bearing bit: ERROR_ACCESS_DENIED (5) must be distinguishable
            // from ERROR_FILE_NOT_FOUND (2) and everything else, so a failed read is never silently
            // read as a deny.
            println!("READER err kind={:?} raw={:?} msg={}", e.kind(), e.raw_os_error(), e);
            match e.raw_os_error() {
                Some(5) => std::process::exit(5),
                _ => std::process::exit(9),
            }
        }
    }
}
