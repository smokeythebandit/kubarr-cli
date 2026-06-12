use std::env;

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

pub fn print_banner() {
    println!("{}", paint("Kubarr Bootstrap", BOLD));
    println!(
        "{}",
        paint("Validate your cluster, then install Kubarr.", DIM)
    );
    println!(
        "{}",
        paint("------------------------------------------------", DIM)
    );
}

pub fn step(name: &str, message: &str) {
    println!("\n{} {}", paint("==>", BLUE), paint(name, BOLD));
    println!("    {}", paint(message, DIM));
}

pub fn ok(message: &str) {
    println!("    {} {message}", status_label("ok", GREEN));
}

pub fn warn(message: &str) {
    println!("    {} {message}", status_label("warn", YELLOW));
}

pub fn fail(message: &str) {
    println!("    {} {message}", status_label("fail", RED));
}

pub fn detail(label: &str, value: &str) {
    println!(
        "    {} {value}",
        paint(&format!("{label}:").to_lowercase(), CYAN)
    );
}

pub fn paint(value: &str, style: &str) -> String {
    if env::var_os("NO_COLOR").is_none() {
        format!("{style}{value}{RESET}")
    } else {
        value.to_string()
    }
}

pub fn status_label(value: &str, color: &str) -> String {
    paint(&format!("[{}]", value.to_ascii_uppercase()), color)
}
