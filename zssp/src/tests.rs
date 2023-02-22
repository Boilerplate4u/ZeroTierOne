/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c) ZeroTier, Inc.
 * https://www.zerotier.com/
 */

/*
#[allow(unused_imports)]
#[cfg(test)]
mod tests {
    use std::collections::LinkedList;
    use std::sync::{Arc, Mutex};
    use zerotier_crypto::hash::SHA384;
    use zerotier_crypto::p384::{P384KeyPair, P384PublicKey};
    use zerotier_crypto::random;
    use zerotier_crypto::secret::Secret;
    use zerotier_utils::hex;

    use crate::*;
    use constants::*;

    struct TestHost {
        local_s: P384KeyPair,
        local_s_hash: [u8; 48],
        psk: Secret<64>,
        session: Mutex<Option<Arc<Session<Box<TestHost>>>>>,
        session_id_counter: Mutex<u64>,
        queue: Mutex<LinkedList<Vec<u8>>>,
        key_id: Mutex<[u8; 16]>,
        this_name: &'static str,
        other_name: &'static str,
    }

    impl TestHost {
        fn new(psk: Secret<64>, this_name: &'static str, other_name: &'static str) -> Self {
            let local_s = P384KeyPair::generate();
            let local_s_hash = SHA384::hash(local_s.public_key_bytes());
            Self {
                local_s,
                local_s_hash,
                psk,
                session: Mutex::new(None),
                session_id_counter: Mutex::new(1),
                queue: Mutex::new(LinkedList::new()),
                key_id: Mutex::new([0; 16]),
                this_name,
                other_name,
            }
        }
    }

    impl ApplicationLayer for Box<TestHost> {
        type Data = u32;
        type SessionRef<'a> = Arc<Session<Box<TestHost>>>;
        type IncomingPacketBuffer = Vec<u8>;
        type RemoteAddress = u32;

        const REKEY_RATE_LIMIT_MS: i64 = 0;

        fn get_local_s_public_blob(&self) -> &[u8] {
            self.local_s.public_key_bytes()
        }

        fn get_local_s_public_blob_hash(&self) -> &[u8; 48] {
            &self.local_s_hash
        }

        fn get_local_s_keypair(&self) -> &P384KeyPair {
            &self.local_s
        }

        fn extract_s_public_from_raw(static_public: &[u8]) -> Option<P384PublicKey> {
            P384PublicKey::from_bytes(static_public)
        }

        fn lookup_session<'a>(&self, local_session_id: SessionId) -> Option<Self::SessionRef<'a>> {
            self.session.lock().unwrap().as_ref().and_then(|s| {
                if s.id == local_session_id {
                    Some(s.clone())
                } else {
                    None
                }
            })
        }

        fn check_new_session(&self, _: &ReceiveContext<Self>, _: &Self::RemoteAddress) -> bool {
            true
        }

        fn accept_new_session(&self, _: &ReceiveContext<Self>, _: &u32, _: &[u8], _: &[u8]) -> Option<(SessionId, Secret<64>, Self::Data)> {
            loop {
                let mut new_id = self.session_id_counter.lock().unwrap();
                *new_id += 1;
                return Some((SessionId::new_from_u64_le((*new_id).to_le()).unwrap(), self.psk.clone(), 0));
            }
        }
    }

    #[allow(unused_variables)]
    #[test]
    fn establish_session() {
        let mut data_buf = [0_u8; (1280 - 32) * MAX_FRAGMENTS];
        let mut mtu_buffer = [0_u8; 1280];
        let mut psk: Secret<64> = Secret::default();
        random::fill_bytes_secure(&mut psk.0);

        let alice_host = Box::new(TestHost::new(psk.clone(), "alice", "bob"));
        let bob_host = Box::new(TestHost::new(psk.clone(), "bob", "alice"));
        let alice_rc: Box<ReceiveContext<Box<TestHost>>> = Box::new(ReceiveContext::new(&alice_host));
        let bob_rc: Box<ReceiveContext<Box<TestHost>>> = Box::new(ReceiveContext::new(&bob_host));

        //println!("zssp: size of session (bytes): {}", std::mem::size_of::<Session<Box<TestHost>>>());

        let _ = alice_host.session.lock().unwrap().insert(Arc::new(
            Session::start_new(
                &alice_host,
                |data| bob_host.queue.lock().unwrap().push_front(data.to_vec()),
                SessionId::random(),
                bob_host.local_s.public_key_bytes(),
                &[],
                &psk,
                1,
                mtu_buffer.len(),
                1,
            )
            .unwrap(),
        ));

        for test_loop in 0..256 {
            let time_ticks = (test_loop * 10000) as i64;
            for host in [&alice_host, &bob_host] {
                let send_to_other = |data: &mut [u8]| {
                    if std::ptr::eq(host, &alice_host) {
                        bob_host.queue.lock().unwrap().push_front(data.to_vec());
                    } else {
                        alice_host.queue.lock().unwrap().push_front(data.to_vec());
                    }
                };

                let rc = if std::ptr::eq(host, &alice_host) {
                    &alice_rc
                } else {
                    &bob_rc
                };

                loop {
                    if let Some(qi) = host.queue.lock().unwrap().pop_back() {
                        let qi_len = qi.len();
                        let r = rc.receive(host, &0, send_to_other, &mut data_buf, qi, mtu_buffer.len(), time_ticks);
                        if r.is_ok() {
                            let r = r.unwrap();
                            match r {
                                ReceiveResult::Ok => {
                                    //println!("zssp: {} => {} ({}): Ok", host.other_name, host.this_name, qi_len);
                                }
                                ReceiveResult::OkData(data) => {
                                    //println!("zssp: {} => {} ({}): OkData length=={}", host.other_name, host.this_name, qi_len, data.len());
                                    assert!(!data.iter().any(|x| *x != 0x12));
                                }
                                ReceiveResult::OkNewSession(new_session) => {
                                    println!("zssp: new session at {} ({})", host.this_name, u64::from(new_session.id));
                                    let mut hs = host.session.lock().unwrap();
                                    assert!(hs.is_none());
                                    let _ = hs.insert(Arc::new(new_session));
                                }
                                ReceiveResult::Ignored => {
                                    println!("zssp: {} => {} ({}): Ignored", host.other_name, host.this_name, qi_len);
                                }
                            }
                        } else {
                            println!(
                                "zssp: {} => {} ({}): error: {}",
                                host.other_name,
                                host.this_name,
                                qi_len,
                                r.err().unwrap().to_string()
                            );
                            panic!();
                        }
                    } else {
                        break;
                    }
                }

                data_buf.fill(0x12);
                if let Some(session) = host.session.lock().unwrap().as_ref().cloned() {
                    if session.established() {
                        {
                            let mut key_id = host.key_id.lock().unwrap();
                            let security_info = session.status().unwrap();
                            if !security_info.0.eq(key_id.as_ref()) {
                                *key_id = security_info.0;
                                println!(
                                    "zssp: new key at {}: fingerprint {} ratchet {} kyber {} latest role {}",
                                    host.this_name,
                                    hex::to_string(key_id.as_ref()),
                                    security_info.1,
                                    security_info.3,
                                    match security_info.2 {
                                        Role::Alice => "A",
                                        Role::Bob => "B",
                                    }
                                );
                            }
                        }
                        for _ in 0..4 {
                            assert!(session
                                .send(
                                    send_to_other,
                                    &mut mtu_buffer,
                                    &data_buf[..((random::xorshift64_random() as usize) % data_buf.len())]
                                )
                                .is_ok());
                        }
                        if (test_loop % 13) == 0 && test_loop > 0 {
                            session.service(host, send_to_other, &[], mtu_buffer.len(), time_ticks, true);
                        }
                    }
                }
            }
        }
    }
}
*/
