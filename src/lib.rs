mod error;
mod proof_of_fairness;

use crate::error::{FsDkrError, FsDkrResult};
use crate::proof_of_fairness::{FairnessProof, FairnessStatement, FairnessWitness};
use curv::arithmetic::{Samplable, Zero};
use curv::cryptographic_primitives::secret_sharing::feldman_vss::VerifiableSS;
use curv::elliptic::curves::secp256_k1::{Secp256k1Point, Secp256k1Scalar};
use curv::elliptic::curves::traits::{ECPoint, ECScalar};
use curv::BigInt;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::keygen::LocalKey;
use paillier::{
    Add, Decrypt, Encrypt, EncryptWithChosenRandomness, Paillier, Randomness, RawCiphertext,
    RawPlaintext,
};
use rayon::prelude::*;
use std::fmt::Debug;
use zeroize::Zeroize;

// Everything here can be broadcastes
#[derive(Clone, PartialEq)]
pub struct RefreshMessage<P> {
    fairness_proof_vec: Vec<FairnessProof<P>>,
    coefficients_committed_vec: VerifiableSS<P>,
    points_committed_vec: Vec<P>,
    points_encrypted_vec: Vec<BigInt>,
}

impl<P> RefreshMessage<P> {
    pub fn distribute(old_key: &LocalKey) -> Self
    where
        P: ECPoint<Scalar = Secp256k1Scalar> + Clone + Zeroize,
        P::Scalar: PartialEq + Clone + Debug,
    {
        let secret = old_key.keys_additive.u_i;
        // secret share old key
        let (vss_scheme, secret_shares) =
            VerifiableSS::<P>::share(old_key.t as usize, old_key.n as usize, &secret);
        // commit to points on the polynomial
        let points_committed_vec: Vec<_> = (0..secret_shares.len())
            .map(|i| P::generator() * secret_shares[i].clone())
            .collect();

        //encrypt points on the polynomial using Paillier keys
        let (points_encrypted_vec, randomness_vec): (Vec<_>, Vec<_>) = (0..secret_shares.len())
            .map(|i| {
                let randomness = BigInt::sample_below(&old_key.paillier_key_vec[i].n);
                let ciphertext = Paillier::encrypt_with_chosen_randomness(
                    &old_key.paillier_key_vec[i],
                    RawPlaintext::from(secret_shares[i].to_big_int().clone()),
                    &Randomness::from(randomness.clone()),
                )
                .0
                .into_owned();
                (ciphertext, randomness)
            })
            .unzip();

        // generate proof of fairness for each {point_committed, point_encrypted} pair
        let fairness_proof_vec: Vec<_> = (0..secret_shares.len())
            .map(|i| {
                let witness = FairnessWitness {
                    x: secret_shares[i].clone(),
                    r: randomness_vec[i].clone(),
                };
                let statement = FairnessStatement {
                    ek: old_key.paillier_key_vec[i].clone(),
                    c: points_encrypted_vec[i].clone(),
                    Y: points_committed_vec[i].clone(),
                };
                FairnessProof::prove(&witness, &statement)
            })
            .collect();

        // TODO: generate a new Paillier key and proof of correct key. add it to broadcast
        RefreshMessage {
            fairness_proof_vec,
            coefficients_committed_vec: vss_scheme,
            points_committed_vec,
            points_encrypted_vec,
        }
    }

    // TODO: change Vec<Self> to slice
    pub fn collect(refresh_messages: &Vec<Self>, old_key: LocalKey) -> FsDkrResult<LocalKey>
    where
        P: ECPoint<Scalar = Secp256k1Scalar> + Clone + Zeroize,
        P::Scalar: PartialEq + Clone + Debug + Zeroize,
    {
        // check we got at least threshold t refresh messages
        if refresh_messages.len() <= old_key.t as usize {
            return Err(FsDkrError::PartiesThresholdViolation {
                threshold: old_key.t,
                refreshed_keys: refresh_messages.len(),
            });
        }

        // check all vectors are of same length
        let reference_len = refresh_messages[0].fairness_proof_vec.len();

        for k in 0..refresh_messages.len() {
            let fairness_proof_len = refresh_messages[k].fairness_proof_vec.len();
            let points_commited_len = refresh_messages[k].points_committed_vec.len();
            let points_encrypted_len = refresh_messages[k].points_encrypted_vec.len();

            if !(fairness_proof_len == reference_len
                && points_commited_len == reference_len
                && points_encrypted_len == reference_len)
            {
                return Err(FsDkrError::SizeMismatchError {
                    refresh_message_index: k,
                    fairness_proof_len,
                    points_commited_len,
                    points_encrypted_len,
                });
            }
        }

        // Tudor: Not sure yet what it means for a refresh message to be duplicated
        for i in 1..refresh_messages.len() {
            if refresh_messages[i..].contains(&refresh_messages[i - 1]) {
                return Err(FsDkrError::DuplicatedRefreshMessage);
            }
        }

        // for each refresh message: check that SUM_j{i^j * C_j} = points_committed_vec[i] for all i
        let refresh_idx = 0..refresh_messages.len();
        let commit_idx = 0..refresh_messages[0].points_committed_vec.len();

        // TODO Tudor: This needs more thinking, currently  there are refresh_messages * commit_points
        // copies happening, might be worth to pin a refresh_message to a thread
        let parallel_indexes: Vec<(Self, usize)> = refresh_idx
            .flat_map(|x| {
                commit_idx
                    .clone()
                    .map(move |y| (refresh_messages[x].clone(), y))
            })
            .collect();

        let invalid_shares: bool =
            parallel_indexes
                .par_iter()
                .any(move |(refresh_message, commit_index)| {
                    //TODO: we should handle the case of t<i<n

                    refresh_message
                        .coefficients_committed_vec
                        .validate_share_public(
                            &refresh_message.points_committed_vec[*commit_index],
                            commit_index + 1,
                        )
                        .is_err()
                });

        if invalid_shares {
            return Err(FsDkrError::PublicShareValidationError);
        }

        // verify all  fairness proofs
        let mut statement: FairnessStatement<P>;
        for k in 0..refresh_messages.len() {
            for i in 0..(old_key.n as usize) {
                //TODO: we should handle the case of t<i<n
                statement = FairnessStatement {
                    ek: old_key.paillier_key_vec[i].clone(),
                    c: refresh_messages[k].points_encrypted_vec[i].clone(),
                    Y: refresh_messages[k].points_committed_vec[i].clone(),
                };
                if refresh_messages[k].fairness_proof_vec[i]
                    .verify(&statement)
                    .is_err()
                {
                    return Err(FsDkrError::FairnessProof);
                }
            }
        }

        //decrypt the new share
        // we first homomorphically add all ciphertext encrypted using our encryption key
        let ciphertext_vec: Vec<_> = (0..refresh_messages.len())
            .map(|k| {
                // TODO: old_key.i fix to general case
                refresh_messages[k].points_encrypted_vec[(old_key.i - 1) as usize].clone()
            })
            .collect();

        let cipher_text_sum = ciphertext_vec.iter().fold(
            Paillier::encrypt(
                &old_key.keys_additive.ek,
                RawPlaintext::from(BigInt::zero()),
            ),
            |acc, x| Paillier::add(&old_key.keys_additive.ek, acc, RawCiphertext::from(x)),
        );

        let new_share = Paillier::decrypt(&old_key.keys_additive.dk, cipher_text_sum)
            .0
            .into_owned();
        println!("new share {:?}", new_share.clone());
        let new_share_fe: P::Scalar = ECScalar::from(&new_share);

        // TODO: check correctness of new Paillier keys and update local key
        // update old key and output new key
        let mut new_key = old_key;
        new_key.keys_linear.x_i = new_share_fe;
        // TODO: fix
        new_key.keys_linear.y = Secp256k1Point::generator() * new_share_fe.clone();

        // TODO: delete old secret keys
        return Ok(new_key);
    }
}

#[cfg(test)]
mod tests {
    use crate::RefreshMessage;
    use curv::cryptographic_primitives::secret_sharing::feldman_vss::{
        ShamirSecretSharing, VerifiableSS,
    };
    use curv::elliptic::curves::secp256_k1::GE;
    use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::keygen::{
        Keygen, LocalKey,
    };
    use round_based::dev::Simulation;

    #[test]
    fn test1() {
        //simulate keygen
        let mut simulation = Simulation::new();
        simulation.enable_benchmarks(false);

        let t = 2;
        let n = 3;
        for i in 1..=n {
            simulation.add_party(Keygen::new(i, t, n).unwrap());
        }
        let old_keys = simulation.run().unwrap();

        let mut broadcast_vec: Vec<RefreshMessage<GE>> = Vec::new();
        for i in 0..n as usize {
            broadcast_vec.push(RefreshMessage::distribute(&old_keys[i]));
        }
        let mut new_keys: Vec<LocalKey> = Vec::new();
        for i in 0..n as usize {
            new_keys.push(RefreshMessage::collect(&broadcast_vec, old_keys[i].clone()).expect(""));
        }
        // check that sum of old keys is equal to sum of new keys
        let old_linear_secret_key: Vec<_> = (0..old_keys.len())
            .map(|i| old_keys[i].keys_linear.x_i)
            .collect();
        let new_linear_secret_key: Vec<_> = (0..new_keys.len())
            .map(|i| new_keys[i].keys_linear.x_i)
            .collect();
        let indices: Vec<_> = (0..old_keys.len()).map(|i| i).collect();
        let vss = VerifiableSS::<GE> {
            parameters: ShamirSecretSharing {
                threshold: t as usize,
                share_count: n as usize,
            },
            commitments: Vec::new(),
        };
        assert_eq!(
            vss.reconstruct(&indices[..], &old_linear_secret_key[..]),
            vss.reconstruct(&indices[..], &new_linear_secret_key[..])
        );
        assert_ne!(old_linear_secret_key, new_linear_secret_key);
        // TODO: generate a signature and check it verifies with the same public  key
    }
}
