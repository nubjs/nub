use libsui::find_section;
use libsui::Elf;
use libsui::Macho;
use libsui::PortableExecutable;

use libsui::utils;

static TEST_ICO: &[u8] = include_bytes!("./tests/test.ico");
const HELP: &str = r#" Usage:
insert new section: sui <sectionname> <exe> <data_file> <output>
extract existing section: sui <sectionname>
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let sectionname = if args.len() < 2 {
        eprintln!("{}", HELP);
        std::process::exit(1);
    } else {
        &args[1]
    };
    if let Some(section) = find_section(sectionname)? {
        println!("Found section");
        println!("{}", std::str::from_utf8(section)?);
        return Ok(());
    }

    if args.len() != 5 {
        eprintln!("{}", HELP);
        std::process::exit(1);
    }
    let exe = &args[2];
    let data_file = &args[3];
    let output = &args[4];
    let exe = std::fs::read(exe)?;
    let data = std::fs::read(data_file)?;

    let mut out = std::fs::File::create(output)?;

    if utils::is_pe(&exe) {
        PortableExecutable::from(&exe)?
            .set_icon(TEST_ICO)?
            .write_resource(sectionname, data)?
            .build(&mut out)?;
    } else if utils::is_macho(&exe) {
        Macho::from(exe)?
            .write_section(sectionname, data)?
            .build_and_sign(&mut out)?;
    } else if utils::is_elf(&exe) {
        Elf::new(&exe).append(sectionname, &data, &mut out)?;
    } else {
        eprintln!("Unsupported file format");
        std::process::exit(1);
    }

    Ok(())
}
