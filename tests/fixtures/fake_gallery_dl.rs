use std::{env,fs,path::PathBuf,process::{Command,Stdio},thread,time::Duration};
fn main() {
    let args:Vec<String>=env::args().collect();
    if args.iter().any(|a|a=="--child") {loop {thread::sleep(Duration::from_secs(1));}}
    if args.iter().any(|a|a=="-j") {println!("[]");return;}
    let dest=PathBuf::from(&args[args.iter().position(|a|a=="-D").unwrap()+1]);
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("download.jpg"),b"completed file").unwrap();
    if args.iter().any(|a|a.contains("wait")) {
        let child=Command::new(env::current_exe().unwrap()).arg("--child").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        fs::write(dest.join("child.pid"),child.id().to_string()).unwrap();
        loop {thread::sleep(Duration::from_secs(1));}
    }
    if args.iter().any(|a|a.contains("failure")) {eprintln!("HTTP 503 network/download failure");std::process::exit(1);}
    // This text must not be interpreted as application shutdown.
    eprintln!("KeyboardInterrupt is just diagnostic text in this fixture");
}
