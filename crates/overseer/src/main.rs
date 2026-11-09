fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        overseer_gui::run_gui_spike();
    } else {
        eprintln!("unknown arguments: {}", args.join(" "));
        std::process::exit(2);
    }
}
