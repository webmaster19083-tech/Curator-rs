fn main() {
    tauri_build::build();
    slint_build::compile("ui/main.slint").expect("could not compile Curator Slint shell");
}
