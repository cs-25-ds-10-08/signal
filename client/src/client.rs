use crate::{
    contact_manager::ContactManager,
    encryption::{encrypt, pad_message},
    errors::{
        DatabaseError, ProcessPreKeyBundleError, ReceiveMessageError, Result, SendMessageError,
        SignalClientError,
    },
    key_manager::KeyManager,
    server::{SignalServer, SignalServerAPI},
    storage::{
        database::ClientDB,
        device::Device,
        generic::{ProtocolStore, Storage},
    },
};
use axum::http::StatusCode;
use base64::{prelude::BASE64_STANDARD, Engine as _};
use common::{
    envelope::ProcessedEnvelope,
    signalservice::{envelope, Content, DataMessage, Envelope, WebSocketResponseMessage},
    web_api::{AccountAttributes, RegistrationRequest, SignalMessage, SignalMessages},
};
use core::str;
use libsignal_core::{Aci, DeviceId, Pni, ProtocolAddress, ServiceId};
use libsignal_protocol::{process_prekey_bundle, CiphertextMessage, IdentityKeyPair};
use prost::Message;
use rand::{rngs::OsRng, Rng};
use sqlx::{migrate::MigrateDatabase, Sqlite, SqlitePool};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::HashMap, thread, time::Duration};

pub struct Client<T: ClientDB, U: SignalServerAPI> {
    pub aci: Aci,
    #[allow(unused)]
    pub pni: Pni,
    contact_manager: ContactManager,
    server_api: U,
    key_manager: KeyManager,
    pub storage: Storage<T>,
}

const PROFILE_KEY_LENGTH: usize = 32;
const MASTER_KEY_LENGTH: usize = 32;
const PASSWORD_LENGTH: usize = 16;

impl<T: ClientDB, U: SignalServerAPI> Client<T, U> {
    fn new(
        aci: Aci,
        pni: Pni,
        contact_manager: ContactManager,
        server_api: U,
        key_manager: KeyManager,
        storage: Storage<T>,
    ) -> Self {
        Client {
            aci,
            pni,
            contact_manager,
            server_api,
            key_manager,
            storage,
        }
    }

    async fn connect_to_db(database_url: &str, create_db: bool) -> Result<SqlitePool> {
        if create_db {
            if !Sqlite::database_exists(database_url).await.unwrap_or(false) {
                Sqlite::create_database(database_url)
                    .await
                    .expect(&format!("Could not open db: {}", database_url));
            } else {
                return Err(DatabaseError::AlreadyExists.into());
            }
        }
        let pool = SqlitePool::connect(database_url).await.unwrap();

        sqlx::migrate!("client_db/migrations")
            .run(&pool)
            .await
            .expect("Could not run migrations");

        Ok(pool)
    }

    /// Register a new account with the server.
    /// `phone_number` must be unique.
    pub async fn register(
        name: &str,
        phone_number: String,
        database_url: &str,
        server_url: &str,
        cert_path: &Option<String>,
    ) -> Result<Client<Device, SignalServer>> {
        let mut csprng = OsRng;
        let aci_registration_id = OsRng.gen_range(1..16383);
        let pni_registration_id = OsRng.gen_range(1..16383);
        let aci_id_key_pair = IdentityKeyPair::generate(&mut csprng);
        let pni_id_key_pair = IdentityKeyPair::generate(&mut csprng);
        let pool = Client::<T, U>::connect_to_db(database_url, true).await?;
        let device = Device::new(pool);
        device
            .insert_account_key_information(aci_id_key_pair, aci_registration_id)
            .await
            .unwrap();

        let mut proto_storage = ProtocolStore::new(device.clone());
        let mut key_manager = KeyManager::default();

        let aci_signed_pk = key_manager
            .generate_signed_pre_key(
                &mut proto_storage.identity_key_store,
                &mut proto_storage.signed_pre_key_store,
                &mut csprng,
            )
            .await?;

        let pni_signed_pk = key_manager
            .generate_signed_pre_key(
                &mut proto_storage.identity_key_store,
                &mut proto_storage.signed_pre_key_store,
                &mut csprng,
            )
            .await?;

        let aci_pq_last_resort = key_manager
            .generate_kyber_pre_key(
                &mut proto_storage.identity_key_store,
                &mut proto_storage.kyber_pre_key_store,
            )
            .await?;

        let pni_pq_last_resort = key_manager
            .generate_kyber_pre_key(
                &mut proto_storage.identity_key_store,
                &mut proto_storage.kyber_pre_key_store,
            )
            .await?;

        let mut password = [0u8; PASSWORD_LENGTH];
        csprng.fill(&mut password);
        let password = BASE64_STANDARD.encode(password);
        let password = password[0..password.len() - 2].to_owned();

        let mut profile_key = [0u8; PROFILE_KEY_LENGTH];
        csprng.fill(&mut profile_key);

        let access_key = [0u8; 16]; // This should be derived from profile_key

        let mut master_key = [0u8; MASTER_KEY_LENGTH];
        csprng.fill(&mut master_key);

        let account_attributes = AccountAttributes::new(
            name.into(),
            true,
            aci_registration_id,
            pni_registration_id,
            Vec::new(),
            Box::new(access_key),
        );
        let mut server_api = SignalServer::new(cert_path, server_url);

        let req = RegistrationRequest::new(
            "".into(),
            "".into(),
            account_attributes,
            true, // Require atomic is always true
            true, // Skip device transfer is always true
            *aci_id_key_pair.identity_key(),
            *pni_id_key_pair.identity_key(),
            aci_signed_pk.into(),
            pni_signed_pk.into(),
            aci_pq_last_resort.into(),
            pni_pq_last_resort.into(),
            None,
            None,
        );

        let response = server_api
            .register_client(phone_number, password.clone(), req, None)
            .await?;

        let aci: Aci = response.uuid.into();
        let pni: Pni = response.pni.into();

        server_api.create_auth_header(aci, password.clone(), 1.into());

        let contact_manager = ContactManager::new();
        device
            .insert_account_information(aci, pni, password.clone())
            .await
            .map_err(DatabaseError::from)?;
        let mut storage = Storage::new(device.clone(), proto_storage);
        let key_bundle = key_manager
            .generate_key_bundle(&mut storage.protocol_store)
            .await?;

        server_api.publish_pre_key_bundle(key_bundle).await?;

        println!("Connecting to {}...", server_url);
        server_api
            .connect(&aci.service_id_string(), &password, server_url, cert_path)
            .await?;
        println!("Connected");

        Ok(Client::new(
            aci,
            pni,
            contact_manager,
            server_api,
            key_manager,
            storage,
        ))
    }

    pub async fn login(
        database_url: &str,
        cert_path: &Option<String>,
        server_url: &str,
    ) -> Result<Client<Device, SignalServer>> {
        let pool = Client::<T, U>::connect_to_db(database_url, false).await?;
        let device = Device::new(pool);
        let contacts = device
            .load_contacts()
            .await
            .map(|contacts| {
                let mut c = HashMap::new();
                for contact in contacts {
                    c.insert(contact.service_id, contact);
                }
                c
            })
            .map_err(DatabaseError::from)?;
        let (one_time, signed, kyber) = device.get_key_ids().await.map_err(DatabaseError::from)?;

        let password = device.get_password().await.map_err(DatabaseError::from)?;
        let aci = device.get_aci().await.map_err(DatabaseError::from)?;

        let mut server_api = SignalServer::new(cert_path, server_url);

        server_api
            .connect(&aci.service_id_string(), &password, server_url, cert_path)
            .await?;

        server_api.create_auth_header(aci, password.clone(), 1.into());

        Ok(Client::new(
            device.get_aci().await.map_err(DatabaseError::from)?,
            device.get_pni().await.map_err(DatabaseError::from)?,
            ContactManager::new_with_contacts(contacts),
            server_api,
            KeyManager::new(signed + 1, kyber + 1, one_time + 1), // Adds 1 to prevent reusing key ids
            Storage::new(device.clone(), ProtocolStore::new(device.clone())),
        ))
    }

    pub async fn disconnect(&mut self) {
        self.server_api.disconnect().await;
    }

    pub async fn send_message(&mut self, message: &str, service_id: &ServiceId) -> Result<()> {
        let msgs = &self.create_message(message, service_id).await?;

        match self.server_api.send_msg(msgs, &service_id).await {
            Err(SignalClientError::SendMessageError(SendMessageError::WebSocketMessageError(
                Some(err),
            ))) if err
                .headers
                .iter()
                .filter(|header| header.contains("\"extraDevices\":[]"))
                .collect::<Vec<_>>()
                .len()
                > 0 =>
            {
                self.update_contact(service_id).await?;
                let msgs = &self.create_message(message, service_id).await?;
                self.server_api.send_msg(msgs, &service_id).await
            }
            other_results => other_results,
        }
    }

    pub async fn create_message(
        &mut self,
        message: &str,
        service_id: &ServiceId,
    ) -> Result<SignalMessages> {
        let content = Content::builder()
            .data_message(
                DataMessage::builder()
                    .body(message.to_owned())
                    .contact(vec![])
                    .body_ranges(vec![])
                    .preview(vec![])
                    .attachments(vec![])
                    .build(),
            )
            .build();

        let timestamp = SystemTime::now();

        let msgs = encrypt(
            &mut self.storage.protocol_store.identity_key_store,
            &mut self.storage.protocol_store.session_store,
            self.contact_manager.get_contact(&service_id)?,
            pad_message(content.encode_to_vec().as_ref()).as_ref(),
            timestamp,
        )
        .await?;

        // Put messages into structure ready.
        Ok(SignalMessages {
            messages: msgs
                .into_iter()
                .map(|(id, msg)| SignalMessage {
                    r#type: match msg.1 {
                        CiphertextMessage::SignalMessage(_) => envelope::Type::Ciphertext.into(),
                        CiphertextMessage::SenderKeyMessage(_) => {
                            envelope::Type::KeyExchange.into()
                        }
                        CiphertextMessage::PreKeySignalMessage(_) => {
                            envelope::Type::PrekeyBundle.into()
                        }
                        CiphertextMessage::PlaintextContent(_) => {
                            envelope::Type::PlaintextContent.into()
                        }
                    },
                    destination_device_id: id.into(),
                    destination_registration_id: msg.0,
                    content: BASE64_STANDARD.encode(msg.1.serialize()),
                })
                .collect(),
            online: true,
            urgent: false,
            timestamp: timestamp
                .duration_since(UNIX_EPOCH)
                .expect("can get the time since epoch")
                .as_secs(),
        })
    }

    pub async fn receive_message(&mut self) -> Result<ProcessedEnvelope> {
        // I get Envelope from Server.
        let request = self
            .server_api
            .get_message()
            .await
            .ok_or(ReceiveMessageError::NoMessageReceived)?;

        let envelope = match Envelope::decode(request.body()) {
            Ok(e) => e,
            Err(_) => {
                self.server_api
                    .send_response(request, StatusCode::INTERNAL_SERVER_ERROR)
                    .await?;

                return Err(ReceiveMessageError::EnvelopeDecodeError)?;
            }
        };
        let processed = Envelope::decrypt(
            envelope,
            &mut self.storage.protocol_store.session_store,
            &mut self.storage.protocol_store.identity_key_store,
            &mut self.storage.protocol_store.pre_key_store,
            &mut self.storage.protocol_store.signed_pre_key_store,
            &mut self.storage.protocol_store.kyber_pre_key_store,
            &mut OsRng,
        )
        .await?;

        let _ = self.server_api.send_response(request, StatusCode::OK).await;

        // The final message is stored within a DataMessage inside a Content.
        Ok(processed)
    }

    async fn update_contact(&mut self, service_id: &ServiceId) -> Result<()> {
        let device_ids = self.update_device_ids(&service_id).await?;
        self.contact_manager
            .update_contact(&service_id, device_ids)
            .map_err(SignalClientError::ContactManagerError)?;

        let contact = self
            .contact_manager
            .get_contact(&service_id)
            .map_err(SignalClientError::ContactManagerError)?;

        self.storage
            .device
            .store_contact(contact)
            .await
            .map_err(|err| DatabaseError::Custom(Box::new(err)).into())
    }

    async fn update_device_ids(&mut self, service_id: &ServiceId) -> Result<Vec<DeviceId>> {
        let bundles = self.server_api.fetch_pre_key_bundles(service_id).await?;

        let mut device_ids = Vec::new();
        let time = SystemTime::now();
        for ref bundle in bundles {
            // Device id is safe to unwrap.
            let device_id = bundle.device_id().unwrap();
            device_ids.push(device_id);
            process_prekey_bundle(
                &ProtocolAddress::new(service_id.service_id_string(), device_id),
                &mut self.storage.protocol_store.session_store,
                &mut self.storage.protocol_store.identity_key_store,
                bundle,
                time,
                &mut OsRng,
            )
            .await
            .map_err(ProcessPreKeyBundleError)?;
        }

        Ok(device_ids)
    }
}
