#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() -> impl std::process::Termination {
    pawcommander::run(std::env::args_os().skip(1))
}
