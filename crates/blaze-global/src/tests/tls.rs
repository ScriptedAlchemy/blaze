//! The real TLS channels and signed bundle protocol over the same seeded IO driver as
//! the other cache tests. These exercise no compiler: they prove the transport and store
//! boundary, not D11's cold compiler build or its critical-path latency claim.

use std::sync::Arc;

use super::*;
use crate::{TlsClient, TlsServer};

type TlsWorld = World<TlsClient, TlsServer>;

fn world(seed: u64, host: &str, pin_certificate: bool) -> TlsWorld {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let der = certificate.der().clone();
    let mut roots = rustls::RootCertStore::empty();
    if pin_certificate {
        roots.add(der.clone()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client_config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![der],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    let mut effects = Vec::new();
    let mine = Store::open(store_config("mine", MINE), &mut effects);
    let cache = Store::open(store_config("cache", CACHE), &mut effects);
    let client = Client::new(
        Endpoint { at: at(), host: host.to_owned() },
        TlsClient::with_config(Arc::new(client_config), host).unwrap(),
        trust(),
        CLIENT,
    );
    let mut server = Server::new(at(), TlsServer::new(Arc::new(server_config)), trust(), SERVER);
    server.start(&mut effects);
    let mut world = World {
        sim: Sim::new(seed, Faults::default()),
        effects,
        mine,
        client,
        cache: Some(Cache { store: cache, server }),
        raw: None,
        download: None,
        fetched: Vec::new(),
        published: Vec::new(),
        bundles: Vec::new(),
    };
    world.run();
    assert!(world.mine.ready(), "{:?}", world.mine.broken());
    assert!(world.cache().store.ready());
    world
}

fn download(world: &mut TlsWorld, id: BundleId) -> Result<Downloaded, String> {
    world.download = Some(Download::start(id, &mut world.client, &mut world.effects));
    world.run();
    world.download.take().unwrap().done().expect("the bundle request settled").clone()
}

/// Plants a server-side manifest directly. The negative controls deliberately bypass the
/// publisher's checks; the receiving client must still verify everything it admits.
fn seed_manifest(world: &mut TlsWorld, manifest: &Manifest) {
    let mut bytes = Vec::new();
    manifest.encode(&mut bytes);
    let cache = &mut world.cache.as_mut().unwrap().store;
    cache.put(manifest.id.manifest_key(), Kind::Query, &bytes).unwrap();
    cache.flush(&mut world.effects);
    world.run();
}

#[test]
fn tls_bundle_publication_downloads_into_an_empty_local_store() {
    let mut world = world(300, "localhost", true);
    let values = values();
    let secrets = Secrets::new();
    for (key, kind, bytes) in &values {
        world
            .client
            .publish(&public(*key, *kind), bytes.clone(), &publisher(1), &secrets, &mut world.effects)
            .unwrap();
    }
    world.run();
    assert_eq!(world.published.len(), values.len());
    assert!(world.published.iter().all(|answer| answer.result.is_ok()), "{:?}", world.published);
    let manifest = manifest(&values);
    world.client.publish_bundle(manifest.clone(), &mut world.effects);
    world.run();
    assert_eq!(world.published.len(), values.len() + 1);
    assert!(world.published.iter().all(|answer| answer.result.is_ok()), "{:?}", world.published);
    assert!(world.published.iter().any(|answer| answer.key == Key(manifest.id.0)));
    for (key, _, _) in &values {
        assert!(!world.mine.has(key), "publishing must not prewarm the receiving store");
    }

    let total_bytes = values.iter().map(|(_, _, bytes)| bytes.len() as u64).sum::<u64>();
    let result = download(&mut world, manifest.id).unwrap();
    let Downloaded::Kept { listed, fetched, bytes, tickets } = result else {
        panic!("the published bundle was missing")
    };
    assert_eq!((listed, fetched, bytes), (values.len(), values.len(), total_bytes));
    assert_eq!(tickets.len(), values.len());
    for (key, _, bytes) in &values {
        assert_eq!(world.kept(key).as_ref(), Some(bytes));
    }
    assert_eq!(world.fetched.len(), values.len(), "the bundle shares fetch outcomes with other owners");

    // Removing the cold-store cause removes all value downloads, while the same real
    // client still verifies the manifest over TLS. Readable values still carry the last
    // pending put of their segment until it has actually been flushed.
    let again = download(&mut world, manifest.id).unwrap();
    assert_eq!(again, Downloaded::Kept {
        listed: values.len(), fetched: 0, bytes: 0,
        tickets: vec![*tickets.last().unwrap()],
    });
    world.mine.flush(&mut world.effects);
    world.run();
    assert!(tickets.iter().all(|ticket| world.mine.durable(*ticket)));
    let durable = download(&mut world, manifest.id).unwrap();
    assert_eq!(durable, Downloaded::Kept { listed: values.len(), fetched: 0, bytes: 0, tickets: Vec::new() });
}

#[test]
fn tls_bundle_refuses_an_untrusted_certificate_or_wrong_server_name() {
    for (host, pin_certificate) in [("localhost", false), ("another.example", true)] {
        let mut world = world(301, host, pin_certificate);
        let values = values();
        for (key, kind, bytes) in &values {
            world.seed(*key, *kind, bytes);
        }
        let manifest = manifest(&values);
        seed_manifest(&mut world, &manifest);
        let why = download(&mut world, manifest.id).unwrap_err();
        assert!(why.contains("TLS:"), "{host}, pinned={pin_certificate}: {why}");
        for (key, _, _) in &values {
            assert_eq!(world.kept(key), None, "a failed TLS peer must admit no value");
        }
    }
}

#[test]
fn tls_bundle_refuses_a_manifest_with_an_unpinned_or_damaged_signature() {
    for unpinned in [true, false] {
        let mut world = world(302, "localhost", true);
        let values = values();
        for (key, kind, bytes) in &values {
            world.seed(*key, *kind, bytes);
        }
        let mut manifest = Manifest::sign(
            BundleId::of(&spec()),
            values.iter().map(|(key, kind, bytes)| (public(*key, *kind), Record::of(*key, *kind, bytes))),
            &publisher(if unpinned { 2 } else { 1 }),
        );
        if !unpinned {
            manifest.signature[0] ^= 1;
        }
        seed_manifest(&mut world, &manifest);
        let why = download(&mut world, manifest.id).unwrap_err();
        assert!(why.contains(if unpinned { "does not pin" } else { "signature" }), "{why}");
        for (key, _, _) in &values {
            assert_eq!(world.kept(key), None, "TLS alone does not authorize a bundle");
        }
    }
}

#[test]
fn tls_values_need_a_pinned_record_or_a_matching_trusted_manifest() {
    for with_manifest in [false, true] {
        let mut world = world(303, "localhost", true);
        let values = vec![(key(700), Kind::Query, b"one signed value".to_vec())];
        let (key, kind, bytes) = &values[0];
        let signed = publisher(2).sign(Record::of(*key, *kind, bytes));
        let cache = &mut world.cache.as_mut().unwrap().store;
        cache.put(*key, *kind, bytes).unwrap();
        cache.put(record_key(key), Kind::Query, &signed.encode()).unwrap();
        cache.flush(&mut world.effects);
        world.run();
        if with_manifest {
            // The pinned publisher signed this exact record as part of the manifest.
            // That signature authorizes it without trusting the per-value publisher.
            let manifest = manifest(&values);
            seed_manifest(&mut world, &manifest);
            let downloaded = download(&mut world, manifest.id).unwrap();
            assert!(matches!(downloaded, Downloaded::Kept { listed: 1, fetched: 1, .. }));
            assert_eq!(world.kept(key).as_ref(), Some(bytes));
        } else {
            world.fetch(&[public(*key, *kind)]);
            let why = world.refused(key);
            assert!(why.contains("does not pin"), "{why}");
            assert_eq!(world.kept(key), None, "TLS alone does not authorize a value record");
        }
    }
}

#[test]
fn tls_bundle_refuses_a_value_not_matching_its_verified_manifest() {
    let mut world = world(304, "localhost", true);
    let values = vec![(key(701), Kind::Query, b"the manifest's value".to_vec())];
    let (key, kind, _) = &values[0];
    // The replacement has a valid publisher signature and matches its server-side
    // bytes. It still is not what the verified manifest promised under this key.
    world.seed(*key, *kind, b"a different correctly signed value");
    let manifest = manifest(&values);
    seed_manifest(&mut world, &manifest);
    let why = download(&mut world, manifest.id).unwrap_err();
    assert!(why.contains("not the one its bundle's manifest lists"), "{why}");
    assert_eq!(world.kept(key), None, "a different signed record must not enter the store");
}
