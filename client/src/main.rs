use client::Client;
use dotenv::dotenv;
use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
use libsignal_core::ServiceId;
use rand::{rngs::OsRng, seq::SliceRandom, Rng};
use server::SignalServer;
use std::{
    collections::HashMap,
    env::{self, var},
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use storage::{device::Device, generic::SignalStore};
use tokio::{sync::Mutex, time::timeout};

mod client;
mod contact_manager;
mod encryption;
mod errors;
mod key_manager;
mod persistent_receiver;
mod server;
mod socket_manager;
mod storage;
#[cfg(test)]
mod test_utils;

fn client_db_path() -> String {
    fs::canonicalize(PathBuf::from("./client_db".to_string()))
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap()
        .replace("\\", "/")
        .trim_start_matches("//?/")
        .to_owned()
}

async fn make_client(
    name: String,
    phone: String,
    certificate_path: Option<String>,
    server_url: String,
) -> Client<Device, SignalServer> {
    let db_path = client_db_path() + "/" + &name + ".db";
    let db_url = format!("sqlite://{}", db_path);
    let client = if Path::exists(Path::new(&db_path)) {
        Client::<Device, SignalServer>::login(&db_url, &certificate_path, &server_url).await
    } else {
        Client::<Device, SignalServer>::register(
            &name,
            phone,
            &db_url,
            &server_url,
            &certificate_path,
        )
        .await
    };
    client.expect("Failed to create client")
}

fn get_server_info() -> (Option<String>, String) {
    let use_tls = !env::args().any(|arg| arg == "--no-tls");
    println!("Using tls: {}", use_tls);
    if use_tls {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("Failed to install rustls crypto provider");
        (
            Some(var("CERT_PATH").expect("Could not find CERT_PATH")),
            var("HTTPS_SERVER_URL").expect("Could not find SERVER_URL"),
        )
    } else {
        (
            None,
            var("HTTP_SERVER_URL").expect("Could not find SERVER_URL"),
        )
    }
}

async fn add_name(
    names: &mut HashMap<String, String>,
    client: &Client<Device, SignalServer>,
    name: &str,
) {
    names.insert(
        client
            .storage
            .get_aci()
            .await
            .expect("No ACI")
            .service_id_string(),
        name.to_owned(),
    );
}

async fn receive_message(
    client: &mut Client<Device, SignalServer>,
    names: &HashMap<String, String>,
    default: &String,
) -> String {
    let msg = client.receive_message().await.expect("Expected Message");
    let name = names
        .get(
            &msg.source_service_id()
                .expect("Failed to decode")
                .service_id_string(),
        )
        .unwrap_or(default);
    let msg_text = msg.try_get_message_as_string().expect("No Text Content");
    // println!("{name}: {msg_text}");
    format!("{}", name)
}

// Random noise
async fn experiment_1() -> Result<(), Box<dyn Error>> {
    dotenv()?;

    let (cert_path, server_url) = get_server_info();

    const ROUNDS: usize = 100;
    const CLIENT_AMOUNT: usize = 100;

    let mut clients = vec![
        make_client(
            format!("client_0"),
            format!("0"),
            cert_path.clone(),
            server_url.clone(),
        )
        .await,
        make_client(
            format!("client_1"),
            format!("1"),
            cert_path.clone(),
            server_url.clone(),
        )
        .await,
    ];
    clients.extend(
        join_all(
            (2..CLIENT_AMOUNT)
                .map(|i| {
                    make_client(
                        format!("client_{}", i),
                        format!("{}", i),
                        cert_path.clone(),
                        server_url.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .await,
    );

    for i in 0..CLIENT_AMOUNT {
        for j in 0..CLIENT_AMOUNT {
            if i == j {
                continue;
            }
            let service_id: ServiceId = clients[j].aci.into();

            clients[i]
                .add_contact(&format!("client_{}", j), &service_id)
                .await
                .expect(&format!("client_{} failed to be added as contact", j));
        }
    }

    let mut contact_names = HashMap::new();
    let default_sender = "Unknown Sender".to_owned();

    for i in 0..CLIENT_AMOUNT {
        add_name(&mut contact_names, &clients[i], &format!("client_{}", i)).await;
    }

    for _ in 0..ROUNDS {
        clients[0]
            .send_message("hello alice, how are you? I am good, i have bouaght a new car, it has 12 cylineders and says wroom wroom", "client_1")
            .await?;
        clients[1].send_message("hello bob", "client_0").await?;

        receive_message(&mut clients[0], &contact_names, &default_sender).await;
        receive_message(&mut clients[1], &contact_names, &default_sender).await;

        let rand1 = rand::thread_rng().gen_range(0..CLIENT_AMOUNT);
        let rand2 = rand::thread_rng().gen_range(0..CLIENT_AMOUNT);

        if rand1 == rand2 {
            continue;
        }

        clients[rand1]
            .send_message("hello", &format!("client_{}", rand2))
            .await?;
        clients[rand2]
            .send_message("hello", &format!("client_{}", rand1))
            .await?;

        receive_message(&mut clients[rand1], &contact_names, &default_sender).await;
        receive_message(&mut clients[rand2], &contact_names, &default_sender).await;
    }

    for i in 0..CLIENT_AMOUNT {
        clients[i].disconnect().await;
    }

    Ok(())
}

// Can only comunicate with clients in its own group
// A client will only be present in one group
async fn experiment_2() -> Result<(), Box<dyn Error>> {
    dotenv()?;

    let (cert_path, server_url) = get_server_info();

    const ROUNDS: usize = 1000;
    const CLIENT_AMOUNT: usize = 100;
    const GROUP_SIZE_MIN: usize = 2;
    const GROUP_SIZE_MAX: usize = 5;

    let mut groups: Vec<Vec<usize>> = vec![];

    let mut clients = vec![
        Arc::new(Mutex::new(
            make_client(
                format!("client_0"),
                format!("0"),
                cert_path.clone(),
                server_url.clone(),
            )
            .await,
        )),
        Arc::new(Mutex::new(
            make_client(
                format!("client_1"),
                format!("1"),
                cert_path.clone(),
                server_url.clone(),
            )
            .await,
        )),
    ];
    clients.extend(
        join_all(
            (2..CLIENT_AMOUNT)
                .map(|i| {
                    make_client(
                        format!("client_{}", i),
                        format!("{}", i),
                        cert_path.clone(),
                        server_url.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .map(|c| Arc::new(Mutex::new(c))),
    );

    for i in 0..CLIENT_AMOUNT {
        for j in 0..CLIENT_AMOUNT {
            if i == j {
                continue;
            }

            let mut clienti = clients[i].lock().await;
            let clientj = clients[j].lock().await;

            let service_id: ServiceId = clientj.aci.into();

            clienti
                .add_contact(&format!("client_{}", j), &service_id)
                .await
                .expect(&format!("client_{} failed to be added as contact", j));
        }
    }

    let mut contact_names = HashMap::new();
    let default_sender = "Unknown Sender".to_owned();

    for i in 0..CLIENT_AMOUNT {
        let clienti = clients[i].lock().await;
        add_name(&mut contact_names, &clienti, &format!("client_{}", i)).await;
    }

    groups.push(vec![0, 1]);

    let mut groups_num = 0;
    let mut i = 2;

    while i < CLIENT_AMOUNT {
        groups.push(vec![]);
        groups_num += 1;
        let group_size = if i + GROUP_SIZE_MAX >= CLIENT_AMOUNT {
            CLIENT_AMOUNT - i
        } else if i + GROUP_SIZE_MAX + GROUP_SIZE_MIN >= CLIENT_AMOUNT {
            (GROUP_SIZE_MAX + GROUP_SIZE_MIN) / 2
        } else {
            rand::thread_rng().gen_range(GROUP_SIZE_MIN..=GROUP_SIZE_MAX)
        };
        // Simpler, but may result in more clients than CLIENT_AMOUNT
        // let group_size = rand::thread_rng().gen_range(GROUP_SIZE_MIN..=GROUP_SIZE_MAX);

        for j in 0..group_size {
            groups[groups_num].push(i + j);
        }

        i += group_size;
    }

    groups
        .iter()
        .map(|group| async {
            for _ in 0..ROUNDS {
                let members = group.choose_multiple(&mut OsRng, 2).collect::<Vec<_>>();
                let client = clients[*members[0]].clone();
                let mut client = client.lock().await;
                let name0 = format!("client_{}", members[0]);
                let name1 = format!("client_{}", members[1]);

                while let Some(receiver) = timeout(
                    Duration::from_millis(100),
                    receive_message(&mut client, &contact_names, &default_sender),
                )
                .await
                .ok()
                {
                    println!("{} <- {}", name0, receiver);
                    if true_by_chance(10) {
                        continue;
                    }
                    println!("{} -> {}", name0, receiver);
                    client
                        .send_message("hello", &receiver)
                        .await
                        .expect("This works");
                }

                println!("{} -> {}", name0, name1);
                if true_by_chance(5) {
                    client
                        .send_message("hello", &name1)
                        .await
                        .expect("This works");
                }
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    for i in 0..CLIENT_AMOUNT {
        let mut clienti = clients[i].lock().await;
        clienti.disconnect().await;
    }

    Ok(())
}

fn true_by_chance(chance: usize) -> bool {
    OsRng.gen_range(0..=100) <= chance
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // experiment_1().await
    experiment_2().await
}
