const BUILD_MARKER: &str = include_str!(concat!(env!("OUT_DIR"), "/build_marker.txt"));

fn main() {
    print!("{BUILD_MARKER}");
    println!("bunkerbox-cargo-fixture-stdout");
    eprintln!("bunkerbox-cargo-fixture-stderr");
}
