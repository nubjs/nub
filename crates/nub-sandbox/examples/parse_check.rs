//! Does nub's catalog validator accept a catalog the corpus collator actually produced?
//!
//! ⛔ THE INTEROP SEAM THAT NO UNIT TEST ON EITHER SIDE COVERS. The collator emits
//! `provenance.generatedAt` and this parser rejects any spelling other than a fixed-width UTC instant,
//! because the ordering it feeds is a LEXICOGRAPHIC comparison. Each side is tested against its own
//! idea of that format; only running the real output through the real parser shows they agree. A
//! mismatch is silent in the worst way — every catalog fails validation and falls back to the compiled
//! floor, so the update mechanism appears to work and never applies.
//!
//! Usage: `cargo run --example parse_check -p nub-sandbox -- <catalog.json>`
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: parse_check <catalog.json>");
    let text = std::fs::read_to_string(&path).expect("read the catalog");
    match nub_sandbox::catalog_v2::parse(&text) {
        Ok(c) => {
            println!(
                "ACCEPTED  generated_at={:?}  packages={}",
                c.generated_at,
                c.packages.len()
            );
            // A catalog that parses but carries no stamp cannot be shown to be newer than anything,
            // so it would never be adopted — reporting that as success would be misleading.
            if c.generated_at.is_none() {
                println!("BUT UNSTAMPED — this catalog could never win the newer-than comparison");
                std::process::exit(2);
            }
        }
        Err(e) => {
            println!("REJECTED  {e}");
            std::process::exit(1);
        }
    }
}
