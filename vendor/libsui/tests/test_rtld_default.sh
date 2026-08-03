rustc --target x86_64-apple-darwin -Clink-arg="-Wl,-exported_symbol,_exported_sym" tests/rtld_default.rs
cargo r -- ./tests/rtld_default tests/test.txt ./tests/out
chmod +x ./tests/out
./tests/out
