//! Manual check against a Docker API (real or mock): list, act, list.
//! Usage: cargo run -p sophon-docker --example probe -- <host:port> [start|stop|restart] [name]
fn main() {
    let mut a = std::env::args().skip(1);
    let addr = a.next().unwrap_or_else(|| "127.0.0.1:2375".into());
    let srv = sophon_docker::Server::parse("probe", &addr).expect("addr");
    let list = || match sophon_docker::list(&srv) {
        Ok(cs) => {
            for c in &cs {
                let ports: Vec<String> = c.ports.iter().map(|p| format!("{}->{}/{}", p.public, p.private, p.proto)).collect();
                println!("  {:<12} {:<10} {:<22} {}", c.name, c.state, c.status, ports.join(" "));
            }
            cs
        }
        Err(e) => {
            eprintln!("list failed: {e}");
            Vec::new()
        }
    };
    println!("before:");
    let cs = list();
    if let (Some(op), Some(name)) = (a.next(), a.next())
        && let Some(c) = cs.iter().find(|c| c.name == name)
    {
        println!("\n{op} {name}: {:?}", sophon_docker::action(&srv, &c.id, &op));
        std::thread::sleep(std::time::Duration::from_millis(200));
        println!("\nafter:");
        list();
    }
}
