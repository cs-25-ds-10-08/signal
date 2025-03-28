use client::Client;
use dotenv::{dotenv, var};
use futures::{future::try_join_all, stream::FuturesUnordered, StreamExt};
use libsignal_core::ServiceId;
use rand::{rngs::OsRng, seq::SliceRandom, Rng};
use server::SignalServer;
use std::{
    env::{self},
    fs, io,
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

type Clients = Vec<Arc<RwLock<Client<Device, SignalServer>>>>;

#[tokio::main]
async fn main() {
    const ROUNDS: usize = 100;
    const CLIENT_AMOUNT: usize = 100;

    #[allow(dead_code)]
    const GROUP_SIZE_MIN: usize = 2;
    #[allow(dead_code)]
    const GROUP_SIZE_MAX: usize = 5;

    let mut error = None;

    match init(CLIENT_AMOUNT).await {
        Ok(clients) => {
            if let Err(e) = experiment_1(ROUNDS, clients).await
            //experiment_2(ROUNDS, clients, GROUP_SIZE_MIN, GROUP_SIZE_MAX).await
            {
                error = Some(e);
            }
        }
        Err(e) => {
            error = Some(e);
        }
    };

    cleanup();
    if error.is_some() {
        println!("ERROR: {}", error.unwrap());
    }
}

// Random noise
#[allow(dead_code)]
async fn experiment_1(rounds: usize, clients: Clients) -> Result<(), String> {
    let service_ids: Arc<Vec<ServiceId>> = Arc::new(
        clients
            .clone()
            .iter()
            .map(|client| async { ServiceId::from(client.read().await.aci) })
            .collect::<FuturesUnordered<_>>()
            .collect()
            .await,
    );

    clients
        .clone()
        .iter()
        .enumerate()
        .map(|(i, client)| {
            let service_ids = service_ids.clone();

            async move {
                let service_ids_len = service_ids.len();
                let target_clients = &service_ids[service_ids_len - 2..service_ids_len];
                let mut client_lock = client.write().await;
                let client_service_id = client_lock.aci.into();
                let mut force_new_conversation = true;

                for r in 0..rounds {
                    //println!("{}", r);
                    while let Some((receiver, _)) = timeout(
                        Duration::from_millis(1000),
                        receive_message(&mut client_lock),
                    )
                    .await
                    .ok()
                    {
                        if true_by_chance(10) {
                            continue;
                        }
                        println!("{r}");
                        force_new_conversation = false;

                        if target_clients.contains(&receiver)
                            == target_clients.contains(&client_service_id)
                        {
                            client_lock
                                .send_message("hello", &receiver)
                                .await
                                .expect("This works");
                        }
                    }

                    if force_new_conversation || true_by_chance(10) {
                        let backup = if target_clients.contains(&client_service_id) {
                            target_clients[i % (service_ids_len - target_clients.len())]
                        } else {
                            loop {
                                let rand = OsRng.gen_range(0..service_ids_len);
                                if rand != i {
                                    break service_ids[rand];
                                }
                            }
                        };
                        client_lock
                            .send_message("hello", &backup)
                            .await
                            .expect("This might works");
                    }
                }
                println!("{}", i);
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    disconnect_clients(clients).await;
    Ok(())
}

// Can only comunicate with clients in its own group
// A client will only be present in one group
#[allow(dead_code)]
async fn experiment_2(
    rounds: usize,
    clients: Clients,
    group_size_min: usize,
    group_size_max: usize,
) -> Result<(), String> {
    let groups = make_groups(clients.len(), group_size_min, group_size_max);

    groups
        .iter()
        .map(|group| async {
            let mut force_new_conversation = true;
            for _ in 0..rounds {
                let members = group.choose_multiple(&mut OsRng, 2).collect::<Vec<_>>();

                let client = clients[*members[0]].clone();
                let mut client = client.write().await;

                let backup: ServiceId = clients[*members[1]].clone().read().await.aci.into();

                while let Ok((receiver, _)) =
                    timeout(Duration::from_millis(100), receive_message(&mut client)).await
                {
                    if true_by_chance(5) {
                        force_new_conversation = true;
                        continue;
                    }
                    force_new_conversation = false;

                    client
                        .send_message("hello", &receiver)
                        .await
                        .expect("This might work");
                }

                if force_new_conversation || true_by_chance(10) {
                    client
                        .send_message("hello", &backup)
                        .await
                        .expect("This might work");
                }
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    disconnect_clients(clients).await;
    Ok(())
}

async fn init(client_amount: usize) -> Result<Clients, String> {
    cleanup();
    dotenv().expect("You need to add a .env file");
    let clients = make_clients(client_amount).await?;

    println!("Ready: press any key to continue");
    let _ = io::stdin().read_line(&mut String::new());
    println!("Starting Experiment");
    Ok(clients)
}

fn cleanup() {
    println!("Cleaning up... 🧹");
    for entry in fs::read_dir("client_db/").expect("Could not read dir") {
        let path = entry.unwrap().path();
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if file_name.starts_with("client_") {
                let _ = fs::remove_file(&path);
            }
        }
    }
    println!("Cleaned up 🧹");
}

async fn make_clients(client_amount: usize) -> Result<Clients, String> {
    let (cert_path, server_url) = get_server_info();
    let mut clients;

    clients = try_join_all((2..client_amount).map(|i| {
        make_client(
            format!("client_{}", i),
            i.to_string(),
            cert_path.clone(),
            server_url.clone(),
        )
    }))
    .await?
    .into_iter()
    .map(|client| Arc::new(RwLock::new(client)))
    .collect::<Vec<_>>();

    clients.push(Arc::new(RwLock::new(
        make_client(
            "client_0".to_string(),
            "0".to_string(),
            cert_path.clone(),
            server_url.clone(),
        )
        .await?,
    )));

    clients.push(Arc::new(RwLock::new(
        make_client(
            "client_1".to_string(),
            "1".to_string(),
            cert_path.clone(),
            server_url.clone(),
        )
        .await?,
    )));

    Ok(clients)
}

async fn make_client(
    name: String,
    phone: String,
    certificate_path: Option<String>,
    server_url: String,
) -> Result<Client<Device, SignalServer>, String> {
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
    client.map_err(|e| format!("Failed to create client: {}", e))
}

#[allow(dead_code)]
fn make_groups(
    clients_amount: usize,
    group_size_min: usize,
    group_size_max: usize,
) -> Vec<Vec<usize>> {
    let mut groups = vec![];
    groups.push(vec![clients_amount - 2, clients_amount - 1]);
    let clients_amount = clients_amount - 2;

    let mut groups_num = 0;
    let mut i = 0;

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

async fn disconnect_clients(clients: Vec<Arc<RwLock<Client<Device, SignalServer>>>>) {
    for client in clients {
        client.write().await.disconnect().await;
    }
}

async fn receive_message(client: &mut Client<Device, SignalServer>) -> (ServiceId, String) {
    let msg = client.receive_message().await.expect("Expected Message");
    (
        msg.source_service_id().expect("Failed to decode"),
        msg.try_get_message_as_string().expect("No Text Content"),
    )
}

fn true_by_chance(chance: usize) -> bool {
    OsRng.gen_range(1..=100) <= chance
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
