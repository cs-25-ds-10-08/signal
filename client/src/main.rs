use client::Client;
use dotenv::dotenv;
use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
use libsignal_core::ServiceId;
use rand::{rngs::OsRng, seq::SliceRandom, Rng};
use server::SignalServer;
use std::{
    env::{self, var},
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use storage::device::Device;
use tokio::{sync::RwLock, time::timeout};

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

async fn disconnect_clients(clients: Vec<Arc<RwLock<Client<Device, SignalServer>>>>) {
    for client in clients {
        client.write().await.disconnect().await;
    }
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

async fn make_clients(client_amount: usize) -> Vec<Arc<RwLock<Client<Device, SignalServer>>>> {
    let (cert_path, server_url) = get_server_info();
    let mut clients = vec![
        Arc::new(RwLock::new(
            make_client(
                format!("client_0"),
                format!("0"),
                cert_path.clone(),
                server_url.clone(),
            )
            .await,
        )),
        Arc::new(RwLock::new(
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
            (2..client_amount)
                .map(|i| {
                    {
                        make_client(
                            format!("client_{}", i),
                            format!("{}", i),
                            cert_path.clone(),
                            server_url.clone(),
                        )
                    }
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .map(|client| Arc::new(RwLock::new(client))),
    );

    clients
}

fn make_groups(
    clients_amount: usize,
    group_size_min: usize,
    group_size_max: usize,
) -> Vec<Vec<usize>> {
    let mut groups = vec![];
    groups.push(vec![0, 1]);

    let mut groups_num = 0;
    let mut i = 2;

    while i < clients_amount {
        groups.push(vec![]);
        groups_num += 1;
        let group_size = if i + group_size_max >= clients_amount {
            clients_amount - i
        } else if i + group_size_max + group_size_min >= clients_amount {
            (group_size_max + group_size_min) / 2
        } else {
            rand::thread_rng().gen_range(group_size_min..=group_size_max)
        };

        // Simpler, but may result in more clients than CLIENT_AMOUNT
        // let group_size = rand::thread_rng().gen_range(GROUP_SIZE_MIN..=GROUP_SIZE_MAX);

        for j in 0..group_size {
            groups[groups_num].push(i + j);
        }

        i += group_size;
    }

    groups
}

async fn receive_message(client: &mut Client<Device, SignalServer>) -> (ServiceId, String) {
    let msg = client.receive_message().await.expect("Expected Message");
    (
        msg.source_service_id().expect("Failed to decode"),
        msg.try_get_message_as_string().expect("No Text Content"),
    )
}

// Random noise
async fn experiment_1() -> Result<(), Box<dyn Error>> {
    dotenv()?;

    const ROUNDS: usize = 100;
    const CLIENT_AMOUNT: usize = 100;

    let clients = Arc::new(make_clients(CLIENT_AMOUNT).await);

    clients
        .iter()
        .enumerate()
        .map(|(i, client)| {
            let clients = clients.clone();
            async move {
                for _ in 0..ROUNDS {
                    while let Some((receiver, _)) = timeout(
                        Duration::from_millis(100),
                        receive_message(&mut *client.write().await),
                    )
                    .await
                    .ok()
                    {
                        if true_by_chance(10) {
                            continue;
                        }
                        client
                            .write()
                            .await
                            .send_message("hello", &receiver)
                            .await
                            .expect("This might works");
                    }

                    if true_by_chance(5) {
                        let random_client_nr = loop {
                            let rand = OsRng.gen_range(0..CLIENT_AMOUNT);
                            if rand != i {
                                break rand;
                            }
                        };

                        let backup = clients[random_client_nr].read().await.aci.into();

                        client
                            .write()
                            .await
                            .send_message("hello", &backup)
                            .await
                            .expect("This might works");
                    }
                }
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    disconnect_clients(clients.to_vec()).await;
    Ok(())
}

// Can only comunicate with clients in its own group
// A client will only be present in one group
async fn experiment_2() -> Result<(), Box<dyn Error>> {
    dotenv()?;

    const ROUNDS: usize = 100;
    const CLIENT_AMOUNT: usize = 100;
    const GROUP_SIZE_MIN: usize = 2;
    const GROUP_SIZE_MAX: usize = 5;

    let clients = make_clients(CLIENT_AMOUNT).await;
    let groups = make_groups(CLIENT_AMOUNT, GROUP_SIZE_MIN, GROUP_SIZE_MAX);

    groups
        .iter()
        .map(|group| async {
            for _ in 0..ROUNDS {
                let members = group.choose_multiple(&mut OsRng, 2).collect::<Vec<_>>();
                let client = clients[*members[0]].clone();
                let backup = clients[*members[1]].read().await.aci.into();

                while let Some((receiver, _)) = timeout(
                    Duration::from_millis(100),
                    receive_message(&mut *client.write().await),
                )
                .await
                .ok()
                {
                    if true_by_chance(10) {
                        continue;
                    }
                    client
                        .write()
                        .await
                        .send_message("hello", &receiver)
                        .await
                        .expect("This might works");
                }

                if true_by_chance(5) {
                    client
                        .write()
                        .await
                        .send_message("hello", &backup)
                        .await
                        .expect("This might works");
                }
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    disconnect_clients(clients).await;
    Ok(())
}

fn true_by_chance(chance: usize) -> bool {
    OsRng.gen_range(0..=100) <= chance
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    experiment_1().await
    //experiment_2().await
}
