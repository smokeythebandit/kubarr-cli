use std::env;

use crate::install_events;

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";
const RULE: &str = "+------------------------------------------------------------+";

pub fn print_banner() {
    println!("{}", paint(RULE, CYAN));
    println!(
        "{}",
        paint(
            "|                      Kubarr Bootstrap                      |",
            BOLD
        )
    );
    println!(
        "{}",
        paint(
            "|        Cluster -> Storage -> Observability -> Apps         |",
            DIM
        )
    );
    println!("{}", paint(RULE, CYAN));
    println!(
        "{}",
        paint(
            "A guided setup for a clean, Kubernetes-native media stack.",
            DIM
        )
    );
}

pub fn wizard_section(title: &str, message: &str) {
    println!();
    println!("{} {}", paint("::", CYAN), paint(title, BOLD));
    println!("   {}", paint(message, DIM));
}

pub fn step(name: &str, message: &str) {
    if install_events::emit(format!(">> {name}\n   {message}")) {
        return;
    }
    println!();
    println!("{} {}", paint(">>", BLUE), paint(name, BOLD));
    println!("   {}", paint(message, DIM));
}

pub fn ok(message: &str) {
    if install_events::emit(format!("[OK] {message}")) {
        return;
    }
    println!("   {} {message}", status_label("ok", GREEN));
}

pub fn warn(message: &str) {
    if install_events::emit(format!("[WARN] {message}")) {
        return;
    }
    println!("   {} {message}", status_label("warn", YELLOW));
}

pub fn fail(message: &str) {
    if install_events::emit(format!("[FAIL] {message}")) {
        return;
    }
    println!("   {} {message}", status_label("fail", RED));
}

pub fn detail(label: &str, value: &str) {
    if install_events::emit(format!("  - {label}: {value}")) {
        return;
    }
    let label = format!("{}:", label).to_lowercase();
    let padded_label = format!("{label:<18}");
    println!(
        "   {} {} {}",
        paint("-", CYAN),
        paint(&padded_label, CYAN),
        value
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
