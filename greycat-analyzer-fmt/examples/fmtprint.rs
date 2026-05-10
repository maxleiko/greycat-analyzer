fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().expect("usage: fmtprint <file>");
    let s = std::fs::read_to_string(path).expect("read");
    print!("{}", greycat_analyzer_fmt::format(&s));
}
