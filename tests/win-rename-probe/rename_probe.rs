//! Standalone: does `std::fs::rename` — the exact call `swap_extract` makes —
//! succeed on a directory whose descendant DLL is mapped into a live process?
//! Compiled with plain `rustc`, no deps, so it needs no workspace build.

fn main() {
    let mut args = std::env::args().skip(1);
    let from = args.next().expect("usage: rename_probe <from> <to>");
    let to = args.next().expect("usage: rename_probe <from> <to>");
    match std::fs::rename(&from, &to) {
        Ok(()) => println!("RENAME_OK   {from} -> {to}"),
        Err(e) => println!(
            "RENAME_ERR  {from} -> {to} :: raw={:?} kind={:?} msg={e}",
            e.raw_os_error(),
            e.kind()
        ),
    }
}
