use std::collections::BTreeMap;
use std::fs::{metadata, read_dir, remove_file, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Error, Result};
use bellperson::groth16;
use bincode::serialize;
use blstrs::{Bls12, Scalar as Fr};
use ff::Field;
use filecoin_hashers::Hasher;
use filecoin_proofs::{
    add_piece, aggregate_seal_commit_proofs, clear_cache, compute_comm_d, decode_from, encode_into,
    fauxrep_aux, generate_empty_sector_update_proof,
    generate_empty_sector_update_proof_with_vanilla, generate_fallback_sector_challenges,
    generate_partition_proofs, generate_piece_commitment, generate_single_partition_proof,
    generate_single_vanilla_proof, generate_single_window_post_with_vanilla, generate_window_post,
    generate_window_post_with_vanilla, generate_winning_post,
    generate_winning_post_sector_challenge, generate_winning_post_with_vanilla,
    get_num_partition_for_fallback_post, get_seal_inputs, merge_window_post_partition_proofs,
    remove_encoded_data, seal_commit_phase1, seal_commit_phase2, seal_pre_commit_phase1,
    seal_pre_commit_phase2, unseal_range, validate_cache_for_commit,
    validate_cache_for_precommit_phase2, verify_aggregate_seal_commit_proofs,
    verify_empty_sector_update_proof, verify_partition_proofs, verify_seal,
    verify_single_partition_proof, verify_window_post, verify_winning_post, Commitment,
    DefaultTreeDomain, MerkleTreeTrait, PaddedBytesAmount, PieceInfo, PoRepConfig, PoStConfig,
    PoStType, PrivateReplicaInfo, ProverId, PublicReplicaInfo, SealCommitOutput,
    SealPreCommitOutput, SealPreCommitPhase1Output, SectorShape16KiB, SectorShape2KiB,
    SectorShape32KiB, SectorShape4KiB, SectorUpdateConfig, UnpaddedByteIndex, UnpaddedBytesAmount,
    SECTOR_SIZE_16_KIB, SECTOR_SIZE_2_KIB, SECTOR_SIZE_32_KIB, SECTOR_SIZE_4_KIB,
    WINDOW_POST_CHALLENGE_COUNT, WINDOW_POST_SECTOR_COUNT, WINNING_POST_CHALLENGE_COUNT,
    WINNING_POST_SECTOR_COUNT,
};
use fr32::bytes_into_fr;
use log::info;
use memmap2::MmapOptions;
use rand::{random, Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use storage_proofs_core::{api_version::ApiVersion, is_legacy_porep_id, sector::SectorId};
use storage_proofs_update::constants::TreeRHasher;
use tempfile::{tempdir, NamedTempFile, TempDir};

use filecoin_proofs::constants::MAX_LEGACY_REGISTERED_SEAL_PROOF_ID;

#[cfg(feature = "big-tests")]
use filecoin_proofs::{
    SectorShape32GiB, SectorShape512MiB, SectorShape64GiB, SECTOR_SIZE_32_GIB, SECTOR_SIZE_512_MIB,
    SECTOR_SIZE_64_GIB,
};

// Use a fixed PoRep ID, so that the parents cache can be re-used between some tests.
// Note however, that parents caches cannot be shared when testing the differences
// between API v1 and v2 behaviour (since the parent caches will be different for the
// same porep_ids).
const ARBITRARY_POREP_ID_V1_0_0: [u8; 32] = [127; 32];
const ARBITRARY_POREP_ID_V1_1_0: [u8; 32] = [128; 32];
const ARBITRARY_POREP_ID_V1_2_0: [u8; 32] = [129; 32];

const TEST_SEED: [u8; 16] = [
    0x59, 0x62, 0xbe, 0x5d, 0x76, 0x3d, 0x31, 0x8d, 0x17, 0xdb, 0x37, 0x32, 0x54, 0x06, 0xbc, 0xe5,
];

fn to_porep_id_verified(registered_seal_proof: u64, api_version: ApiVersion) -> [u8; 32] {
    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&registered_seal_proof.to_le_bytes());

    assert!(match api_version {
        ApiVersion::V1_0_0 => is_legacy_porep_id(porep_id),
        ApiVersion::V1_1_0 | ApiVersion::V1_2_0 => !is_legacy_porep_id(porep_id),
    });

    porep_id
}

#[test]
#[ignore]
fn test_seal_lifecycle_2kib_base_8() -> Result<()> {
    // The first value is RegisteredSealProof value
    // The second value is the ApiVersion to use
    let test_inputs = vec![
        (0u64, ApiVersion::V1_0_0),
        (MAX_LEGACY_REGISTERED_SEAL_PROOF_ID + 1, ApiVersion::V1_1_0),
        (MAX_LEGACY_REGISTERED_SEAL_PROOF_ID + 1, ApiVersion::V1_2_0),
    ];

    for (porep_id_num, api_version) in test_inputs {
        let porep_id = to_porep_id_verified(porep_id_num, api_version);
        seal_lifecycle::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_upgrade_2kib_base_8() -> Result<()> {
    // The first value is RegisteredSealProof value
    // The second value is the ApiVersion to use
    let test_inputs = vec![
        (0u64, ApiVersion::V1_0_0),
        (MAX_LEGACY_REGISTERED_SEAL_PROOF_ID + 1, ApiVersion::V1_1_0),
        (MAX_LEGACY_REGISTERED_SEAL_PROOF_ID + 1, ApiVersion::V1_2_0),
    ];

    for (porep_id_num, api_version) in test_inputs {
        let porep_id = to_porep_id_verified(porep_id_num, api_version);
        seal_lifecycle_upgrade::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_4kib_base_8() -> Result<()> {
    let test_inputs = vec![
        (ARBITRARY_POREP_ID_V1_0_0, ApiVersion::V1_0_0),
        (ARBITRARY_POREP_ID_V1_1_0, ApiVersion::V1_1_0),
        (ARBITRARY_POREP_ID_V1_2_0, ApiVersion::V1_2_0),
    ];

    for (porep_id, api_version) in test_inputs {
        seal_lifecycle::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_upgrade_4kib_base_8() -> Result<()> {
    let test_inputs = vec![
        (ARBITRARY_POREP_ID_V1_0_0, ApiVersion::V1_0_0),
        (ARBITRARY_POREP_ID_V1_1_0, ApiVersion::V1_1_0),
        (ARBITRARY_POREP_ID_V1_2_0, ApiVersion::V1_2_0),
    ];

    for (porep_id, api_version) in test_inputs {
        seal_lifecycle_upgrade::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_16kib_base_8() -> Result<()> {
    let test_inputs = vec![
        (ARBITRARY_POREP_ID_V1_0_0, ApiVersion::V1_0_0),
        (ARBITRARY_POREP_ID_V1_1_0, ApiVersion::V1_1_0),
        (ARBITRARY_POREP_ID_V1_2_0, ApiVersion::V1_2_0),
    ];

    for (porep_id, api_version) in test_inputs {
        seal_lifecycle::<SectorShape16KiB>(SECTOR_SIZE_16_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_upgrade_16kib_base_8() -> Result<()> {
    let test_inputs = vec![
        (ARBITRARY_POREP_ID_V1_0_0, ApiVersion::V1_0_0),
        (ARBITRARY_POREP_ID_V1_1_0, ApiVersion::V1_1_0),
        (ARBITRARY_POREP_ID_V1_2_0, ApiVersion::V1_2_0),
    ];

    for (porep_id, api_version) in test_inputs {
        seal_lifecycle_upgrade::<SectorShape16KiB>(SECTOR_SIZE_16_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_32kib_base_8() -> Result<()> {
    let test_inputs = vec![
        (ARBITRARY_POREP_ID_V1_0_0, ApiVersion::V1_0_0),
        (ARBITRARY_POREP_ID_V1_1_0, ApiVersion::V1_1_0),
        (ARBITRARY_POREP_ID_V1_2_0, ApiVersion::V1_2_0),
    ];

    for (porep_id, api_version) in test_inputs {
        seal_lifecycle::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_seal_lifecycle_upgrade_32kib_base_8() -> Result<()> {
    let test_inputs = vec![
        (ARBITRARY_POREP_ID_V1_0_0, ApiVersion::V1_0_0),
        (ARBITRARY_POREP_ID_V1_1_0, ApiVersion::V1_1_0),
        (ARBITRARY_POREP_ID_V1_2_0, ApiVersion::V1_2_0),
    ];

    for (porep_id, api_version) in test_inputs {
        seal_lifecycle_upgrade::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, &porep_id, api_version)?;
    }

    Ok(())
}

// These tests are good to run, but take a long time.

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_512mib_porep_id_v1_top_8_0_0_api_v1() -> Result<()> {
    use filecoin_proofs::{SectorShape512MiB, SECTOR_SIZE_512_MIB};
    let porep_id_v1: u64 = 2; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1.to_le_bytes());
    assert!(is_legacy_porep_id(porep_id));
    seal_lifecycle::<SectorShape512MiB>(SECTOR_SIZE_512_MIB, &porep_id, ApiVersion::V1_0_0)
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_512mib_porep_id_v1_top_8_0_0_api_v1_1() -> Result<()> {
    use filecoin_proofs::{SectorShape512MiB, SECTOR_SIZE_512_MIB};
    let porep_id_v1_1: u64 = 7; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1_1.to_le_bytes());
    assert!(!is_legacy_porep_id(porep_id));
    seal_lifecycle::<SectorShape512MiB>(SECTOR_SIZE_512_MIB, &porep_id, ApiVersion::V1_1_0)
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_upgrade_512mib_top_8_0_0_v1_1() -> Result<()> {
    seal_lifecycle_upgrade::<SectorShape512MiB>(
        SECTOR_SIZE_512_MIB,
        &ARBITRARY_POREP_ID_V1_1_0,
        ApiVersion::V1_1_0,
    )
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_32gib_porep_id_v1_top_8_8_0_api_v1() -> Result<()> {
    let porep_id_v1: u64 = 3; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1.to_le_bytes());
    assert!(is_legacy_porep_id(porep_id));
    seal_lifecycle::<SectorShape32GiB>(SECTOR_SIZE_32_GIB, &porep_id, ApiVersion::V1_0_0)
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_32gib_porep_id_v1_1_top_8_8_0_api_v1_1() -> Result<()> {
    let porep_id_v1_1: u64 = 8; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1_1.to_le_bytes());
    assert!(!is_legacy_porep_id(porep_id));
    seal_lifecycle::<SectorShape32GiB>(SECTOR_SIZE_32_GIB, &porep_id, ApiVersion::V1_1_0)
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_upgrade_32gib_top_8_8_0_v1_1() -> Result<()> {
    seal_lifecycle_upgrade::<SectorShape32GiB>(
        SECTOR_SIZE_32_GIB,
        &ARBITRARY_POREP_ID_V1_1_0,
        ApiVersion::V1_1_0,
    )
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_64gib_porep_id_v1_top_8_8_2_api_v1() -> Result<()> {
    let porep_id_v1: u64 = 4; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1.to_le_bytes());
    assert!(is_legacy_porep_id(porep_id));
    seal_lifecycle::<SectorShape64GiB>(SECTOR_SIZE_64_GIB, &porep_id, ApiVersion::V1_0_0)
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_64gib_porep_id_v1_1_top_8_8_2_api_v1_1() -> Result<()> {
    let porep_id_v1_1: u64 = 9; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1_1.to_le_bytes());
    assert!(!is_legacy_porep_id(porep_id));
    seal_lifecycle::<SectorShape64GiB>(SECTOR_SIZE_64_GIB, &porep_id, ApiVersion::V1_1_0)
}

#[cfg(feature = "big-tests")]
#[test]
fn test_seal_lifecycle_upgrade_64gib_top_8_8_2_v1_1() -> Result<()> {
    seal_lifecycle_upgrade::<SectorShape64GiB>(
        SECTOR_SIZE_64_GIB,
        &ARBITRARY_POREP_ID_V1_1_0,
        ApiVersion::V1_1_0,
    )
}

fn seal_lifecycle<Tree: 'static + MerkleTreeTrait>(
    sector_size: u64,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) -> Result<()> {
    let mut rng = XorShiftRng::from_seed(TEST_SEED);
    let prover_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    info!(
        "Creating seal proof with ApiVersion {} and PoRep ID {:?}",
        api_version, porep_id
    );
    let (_, replica, _, _) = create_seal::<_, Tree>(
        &mut rng,
        sector_size,
        prover_id,
        false,
        porep_id,
        api_version,
    )?;
    replica.close()?;

    Ok(())
}

fn seal_lifecycle_upgrade<Tree: 'static + MerkleTreeTrait<Hasher = TreeRHasher>>(
    sector_size: u64,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) -> Result<()> {
    let mut rng = &mut XorShiftRng::from_seed(TEST_SEED);
    let prover_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    info!(
        "Creating seal proof for upgrade with ApiVersion {} and PoRep ID {:?}",
        api_version, porep_id
    );
    let (_, replica, _, _) = create_seal_for_upgrade::<_, Tree>(
        &mut rng,
        sector_size,
        prover_id,
        porep_id,
        api_version,
    )?;
    replica.close()?;

    Ok(())
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_1_2kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 1; // Requires auto-padding

    let porep_id_v1_1: u64 = 5; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1_1.to_le_bytes());
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, &porep_id, proofs_to_aggregate)
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_3_2kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 3; // Requires auto-padding

    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, &porep_id, proofs_to_aggregate)
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_5_2kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 5; // Requires auto-padding

    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, &porep_id, proofs_to_aggregate)
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_257_2kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 257; // Requires auto-padding

    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, &porep_id, proofs_to_aggregate)
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_2_4kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 2;

    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, &porep_id, proofs_to_aggregate)
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_1_32kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 1; // Requires auto-padding

    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, &porep_id, proofs_to_aggregate)
}

#[test]
#[ignore]
fn test_seal_proof_aggregation_818_32kib_porep_id_v1_1_base_8() -> Result<()> {
    let proofs_to_aggregate = 818; // Requires auto-padding

    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
    assert!(!is_legacy_porep_id(porep_id));
    aggregate_proofs::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, &porep_id, proofs_to_aggregate)
}

//#[test]
//#[ignore]
//fn test_seal_proof_aggregation_818_32gib_porep_id_v1_1_base_8() -> Result<()> {
//    let proofs_to_aggregate = 818; // Requires auto-padding
//
//    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
//    assert!(!is_legacy_porep_id(porep_id));
//    let verified = aggregate_proofs::<SectorShape32GiB>(
//        SECTOR_SIZE_32_GIB,
//        &porep_id,
//        ApiVersion::V1_1_0,
//        proofs_to_aggregate,
//    )?;
//    assert!(verified);
//
//    Ok(())
//}

//#[test]
//#[ignore]
//fn test_seal_proof_aggregation_818_64gib_porep_id_v1_1_base_8() -> Result<()> {
//    let proofs_to_aggregate = 818; // Requires auto-padding
//
//    let porep_id = ARBITRARY_POREP_ID_V1_1_0;
//    assert!(!is_legacy_porep_id(porep_id));
//    let verified = aggregate_proofs::<SectorShape64GiB>(
//        SECTOR_SIZE_64_GIB,
//        &porep_id,
//        ApiVersion::V1_1_0,
//        proofs_to_aggregate,
//    )?;
//    assert!(verified);
//
//    Ok(())
//}

//#[test]
//#[ignore]
//fn test_seal_proof_aggregation_1024_2kib_porep_id_v1_1_base_8() -> Result<()> {
//    let proofs_to_aggregate = 1024;
//    inner_test_seal_proof_aggregation_2kib_porep_id_v1_1_base_8(proofs_to_aggregate)
//}
//
//#[test]
//#[ignore]
//fn test_seal_proof_aggregation_65536_2kib_porep_id_v1_1_base_8() -> Result<()> {
//    let proofs_to_aggregate = 65536;
//    inner_test_seal_proof_aggregation_2kib_porep_id_v1_1_base_8(proofs_to_aggregate)
//}

fn aggregate_proofs<Tree: 'static + MerkleTreeTrait>(
    sector_size: u64,
    porep_id: &[u8; 32],
    num_proofs_to_aggregate: usize,
) -> Result<()> {
    let mut rng = XorShiftRng::from_seed(TEST_SEED);
    let prover_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    let api_version = ApiVersion::V1_1_0;
    let aggregate_versions = vec![
        groth16::aggregate::AggregateVersion::V1,
        groth16::aggregate::AggregateVersion::V2,
    ];
    for aggregate_version in aggregate_versions {
        let mut commit_outputs = Vec::with_capacity(num_proofs_to_aggregate);
        let mut commit_inputs = Vec::with_capacity(num_proofs_to_aggregate);
        let mut seeds = Vec::with_capacity(num_proofs_to_aggregate);
        let mut comm_rs = Vec::with_capacity(num_proofs_to_aggregate);

        let (commit_output, commit_input, seed, comm_r) = create_seal_for_aggregation::<_, Tree>(
            &mut rng,
            sector_size,
            prover_id,
            porep_id,
            api_version,
        )?;

        for _ in 0..num_proofs_to_aggregate {
            commit_outputs.push(commit_output.clone());
            commit_inputs.extend(commit_input.clone());
            seeds.push(seed);
            comm_rs.push(comm_r);
        }

        let config = porep_config(sector_size, *porep_id, api_version);
        let aggregate_proof = aggregate_seal_commit_proofs::<Tree>(
            &config,
            &comm_rs,
            &seeds,
            commit_outputs.as_slice(),
            aggregate_version,
        )?;
        assert!(verify_aggregate_seal_commit_proofs::<Tree>(
            &config,
            aggregate_proof.clone(),
            &comm_rs,
            &seeds,
            commit_inputs.clone(),
            aggregate_version,
        )?);

        // This ensures that once we generate an snarkpack proof
        // with one version, it cannot verify with another.
        let conflicting_aggregate_version = match aggregate_version {
            groth16::aggregate::AggregateVersion::V1 => groth16::aggregate::AggregateVersion::V2,
            groth16::aggregate::AggregateVersion::V2 => groth16::aggregate::AggregateVersion::V1,
        };
        assert!(!verify_aggregate_seal_commit_proofs::<Tree>(
            &config,
            aggregate_proof,
            &comm_rs,
            &seeds,
            commit_inputs,
            conflicting_aggregate_version,
        )?);
    }

    Ok(())
}

fn get_layer_file_paths(cache_dir: &tempfile::TempDir) -> Vec<PathBuf> {
    let mut list: Vec<_> = read_dir(cache_dir)
        .unwrap_or_else(|_| panic!("failed to read directory {:?}", cache_dir))
        .filter_map(|entry| {
            let cur = entry.expect("reading directory failed");
            let entry_path = cur.path();
            let entry_str = entry_path.to_str().expect("failed to get string from path");
            if entry_str.contains("data-layer") {
                Some(entry_path.clone())
            } else {
                None
            }
        })
        .collect();
    list.sort();
    list
}

fn clear_cache_dir_keep_data_layer(cache_dir: &TempDir) {
    for entry in read_dir(cache_dir).expect("failed to read directory") {
        let entry_path = entry.expect("failed get directory entry").path();
        if entry_path.is_file() {
            // delete everything except the data-layers
            if !entry_path
                .to_str()
                .expect("failed to get string from path")
                .contains("data-layer")
            {
                remove_file(entry_path).expect("failed to remove file")
            }
        }
    }
}

#[test]
fn test_resumable_seal_skip_proofs_v1() {
    let porep_id_v1: u64 = 0; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1.to_le_bytes());
    assert!(is_legacy_porep_id(porep_id));
    run_resumable_seal::<SectorShape2KiB>(true, 0, &porep_id, ApiVersion::V1_0_0);
    run_resumable_seal::<SectorShape2KiB>(true, 1, &porep_id, ApiVersion::V1_0_0);
}

#[test]
fn test_resumable_seal_skip_proofs_v1_1() {
    let porep_id_v1_1: u64 = 5; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1_1.to_le_bytes());
    assert!(!is_legacy_porep_id(porep_id));
    run_resumable_seal::<SectorShape2KiB>(true, 0, &porep_id, ApiVersion::V1_1_0);
    run_resumable_seal::<SectorShape2KiB>(true, 1, &porep_id, ApiVersion::V1_1_0);
}

#[test]
#[ignore]
fn test_resumable_seal_v1() {
    let porep_id_v1: u64 = 0; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1.to_le_bytes());
    assert!(is_legacy_porep_id(porep_id));
    run_resumable_seal::<SectorShape2KiB>(false, 0, &porep_id, ApiVersion::V1_0_0);
    run_resumable_seal::<SectorShape2KiB>(false, 1, &porep_id, ApiVersion::V1_0_0);
}

#[test]
#[ignore]
fn test_resumable_seal_v1_1() {
    let porep_id_v1_1: u64 = 5; // This is a RegisteredSealProof value

    let mut porep_id = [0u8; 32];
    porep_id[..8].copy_from_slice(&porep_id_v1_1.to_le_bytes());
    assert!(!is_legacy_porep_id(porep_id));
    run_resumable_seal::<SectorShape2KiB>(false, 0, &porep_id, ApiVersion::V1_1_0);
    run_resumable_seal::<SectorShape2KiB>(false, 1, &porep_id, ApiVersion::V1_1_0);
}

/// Create a seal, delete a layer and resume
///
/// The current code works on two layers only. The `layer_to_delete` specifies (zero-based) which
/// layer should be deleted.
fn run_resumable_seal<Tree: 'static + MerkleTreeTrait>(
    skip_proofs: bool,
    layer_to_delete: usize,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) {
    fil_logger::maybe_init();

    let sector_size = SECTOR_SIZE_2_KIB;
    let mut rng = XorShiftRng::from_seed(TEST_SEED);
    let prover_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    let (mut piece_file, piece_bytes) =
        generate_piece_file(sector_size).expect("failed to generate piece file");
    let sealed_sector_file = NamedTempFile::new().expect("failed to created sealed sector file");
    let cache_dir = tempdir().expect("failed to create temp dir");

    let config = porep_config(sector_size, *porep_id, api_version);
    let ticket = rng.gen();
    let sector_id = rng.gen::<u64>().into();

    // First create seals as expected
    run_seal_pre_commit_phase1::<Tree>(
        &config,
        prover_id,
        sector_id,
        ticket,
        &cache_dir,
        &mut piece_file,
        &sealed_sector_file,
    )
    .expect("failed to run seal pre commit phase1");
    let layers = get_layer_file_paths(&cache_dir);
    assert_eq!(layers.len(), 2, "not all expected layers were created");

    // Delete one layer, keep the other
    clear_cache_dir_keep_data_layer(&cache_dir);
    remove_file(&layers[layer_to_delete]).expect("failed to remove layer");
    let layers_remaining = get_layer_file_paths(&cache_dir);
    assert_eq!(layers_remaining.len(), 1, "expected one layer only");
    if layer_to_delete == 0 {
        assert_eq!(layers_remaining[0], layers[1], "wrong layer was removed");
    } else {
        assert_eq!(layers_remaining[0], layers[0], "wrong layer was removed");
    }

    // Resume the seal
    piece_file
        .rewind()
        .expect("failed to seek piece file to start");
    let (piece_infos, phase1_output) = run_seal_pre_commit_phase1::<Tree>(
        &config,
        prover_id,
        sector_id,
        ticket,
        &cache_dir,
        &mut piece_file,
        &sealed_sector_file,
    )
    .expect("failed to run seal pre commit phase1");

    // Running proofs clears the cache, hence we can only check for existence of files if we don't
    // run them
    if skip_proofs {
        let layers_recreated = get_layer_file_paths(&cache_dir);
        assert_eq!(
            layers_recreated.len(),
            2,
            "not all expected layers were recreated"
        );
        assert_eq!(
            layers_recreated, layers,
            "recreated layers don't match original ones"
        );
    } else {
        let pre_commit_output = seal_pre_commit_phase2(
            &config,
            phase1_output,
            cache_dir.path(),
            sealed_sector_file.path(),
        )
        .expect("failed to run seal pre commit phase2");

        validate_cache_for_commit::<_, _, Tree>(cache_dir.path(), sealed_sector_file.path())
            .expect("failed to validate cache for commit");

        let seed = rng.gen();
        proof_and_unseal::<Tree>(
            &config,
            cache_dir.path(),
            &sealed_sector_file,
            prover_id,
            sector_id,
            ticket,
            seed,
            pre_commit_output,
            &piece_infos,
            &piece_bytes,
        )
        .expect("failed to proof");
    }
}

#[test]
#[ignore]
fn test_winning_post_2kib_base_8() -> Result<()> {
    winning_post::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, false, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, true, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, false, ApiVersion::V1_1_0)?;
    winning_post::<SectorShape2KiB>(SECTOR_SIZE_2_KIB, true, ApiVersion::V1_1_0)
}

#[test]
#[ignore]
fn test_winning_post_4kib_sub_8_2() -> Result<()> {
    winning_post::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, false, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, true, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, false, ApiVersion::V1_1_0)?;
    winning_post::<SectorShape4KiB>(SECTOR_SIZE_4_KIB, true, ApiVersion::V1_1_0)
}

#[test]
#[ignore]
fn test_winning_post_16kib_sub_8_8() -> Result<()> {
    winning_post::<SectorShape16KiB>(SECTOR_SIZE_16_KIB, false, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape16KiB>(SECTOR_SIZE_16_KIB, true, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape16KiB>(SECTOR_SIZE_16_KIB, false, ApiVersion::V1_1_0)?;
    winning_post::<SectorShape16KiB>(SECTOR_SIZE_16_KIB, true, ApiVersion::V1_1_0)
}

#[test]
#[ignore]
fn test_winning_post_32kib_top_8_8_2() -> Result<()> {
    winning_post::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, false, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, true, ApiVersion::V1_0_0)?;
    winning_post::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, false, ApiVersion::V1_1_0)?;
    winning_post::<SectorShape32KiB>(SECTOR_SIZE_32_KIB, true, ApiVersion::V1_1_0)
}

#[test]
fn test_winning_post_empty_sector_challenge() -> Result<()> {
    let mut rng = XorShiftRng::from_seed(TEST_SEED);

    let prover_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    let sector_count = 0;
    let sector_size = SECTOR_SIZE_2_KIB;
    let api_version = ApiVersion::V1_1_0;

    let (_, replica, _, _) = create_seal::<_, SectorShape2KiB>(
        &mut rng,
        sector_size,
        prover_id,
        true,
        &ARBITRARY_POREP_ID_V1_1_0,
        api_version,
    )?;

    let random_fr: DefaultTreeDomain = Fr::random(rng).into();
    let mut randomness = [0u8; 32];
    randomness.copy_from_slice(AsRef::<[u8]>::as_ref(&random_fr));

    let config = PoStConfig {
        sector_size: sector_size.into(),
        sector_count,
        challenge_count: WINNING_POST_CHALLENGE_COUNT,
        typ: PoStType::Winning,
        priority: false,
        api_version,
    };

    assert!(generate_winning_post_sector_challenge::<SectorShape2KiB>(
        &config,
        &randomness,
        sector_count as u64,
        prover_id
    )
    .is_err());

    replica.close()?;

    Ok(())
}

fn winning_post<Tree: 'static + MerkleTreeTrait>(
    sector_size: u64,
    fake: bool,
    api_version: ApiVersion,
) -> Result<()> {
    let mut rng = XorShiftRng::from_seed(TEST_SEED);

    let prover_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    let porep_id = match api_version {
        ApiVersion::V1_0_0 => ARBITRARY_POREP_ID_V1_0_0,
        ApiVersion::V1_1_0 => ARBITRARY_POREP_ID_V1_1_0,
        ApiVersion::V1_2_0 => ARBITRARY_POREP_ID_V1_2_0,
    };

    let (sector_id, replica, comm_r, cache_dir) = if fake {
        create_fake_seal::<_, Tree>(&mut rng, sector_size, &porep_id, api_version)?
    } else {
        create_seal::<_, Tree>(
            &mut rng,
            sector_size,
            prover_id,
            true,
            &porep_id,
            api_version,
        )?
    };
    let sector_count = WINNING_POST_SECTOR_COUNT;

    let random_fr: DefaultTreeDomain = Fr::random(&mut rng).into();
    let mut randomness = [0u8; 32];
    randomness.copy_from_slice(AsRef::<[u8]>::as_ref(&random_fr));

    let config = PoStConfig {
        sector_size: sector_size.into(),
        sector_count,
        challenge_count: WINNING_POST_CHALLENGE_COUNT,
        typ: PoStType::Winning,
        priority: false,
        api_version,
    };

    let challenged_sectors = generate_winning_post_sector_challenge::<Tree>(
        &config,
        &randomness,
        sector_count as u64,
        prover_id,
    )?;
    assert_eq!(challenged_sectors.len(), sector_count);
    assert_eq!(challenged_sectors[0], 0); // with a sector_count of 1, the only valid index is 0

    let pub_replicas = vec![(sector_id, PublicReplicaInfo::new(comm_r)?)];
    let private_replica_info =
        PrivateReplicaInfo::new(replica.path().into(), comm_r, cache_dir.path().into())?;

    /////////////////////////////////////////////
    // The following methods of proof generation are functionally equivalent:
    // 1)
    //
    let priv_replicas = vec![(sector_id, private_replica_info.clone())];
    let proof = generate_winning_post::<Tree>(&config, &randomness, &priv_replicas[..], prover_id)?;

    let valid =
        verify_winning_post::<Tree>(&config, &randomness, &pub_replicas[..], prover_id, &proof)?;
    assert!(valid, "proof did not verify");

    //
    // 2)
    let mut vanilla_proofs = Vec::with_capacity(sector_count);
    let challenges =
        generate_fallback_sector_challenges::<Tree>(&config, &randomness, &[sector_id], prover_id)?;

    // Make sure that files can be read-only for a window post.
    set_readonly_flag(replica.path(), true);
    set_readonly_flag(cache_dir.path(), true);

    let single_proof = generate_single_vanilla_proof::<Tree>(
        &config,
        sector_id,
        &private_replica_info,
        &challenges[&sector_id],
    )?;

    vanilla_proofs.push(single_proof);

    let proof = generate_winning_post_with_vanilla::<Tree>(
        &config,
        &randomness,
        prover_id,
        vanilla_proofs,
    )?;
    /////////////////////////////////////////////

    let valid =
        verify_winning_post::<Tree>(&config, &randomness, &pub_replicas[..], prover_id, &proof)?;
    assert!(valid, "proof did not verify");

    // Make files writeable again, so that the temporary directory can be removed.
    set_readonly_flag(replica.path(), false);
    set_readonly_flag(cache_dir.path(), false);

    replica.close()?;

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_single_partition_smaller_2kib_base_8() -> Result<()> {
    let sector_size = SECTOR_SIZE_2_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape2KiB>(
            sector_size,
            sector_count / 2,
            sector_count,
            false,
            version,
        )?;
        window_post::<SectorShape2KiB>(sector_size, sector_count / 2, sector_count, true, version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_two_partitions_matching_2kib_base_8() -> Result<()> {
    let sector_size = SECTOR_SIZE_2_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape2KiB>(
            sector_size,
            2 * sector_count,
            sector_count,
            false,
            version,
        )?;
        window_post::<SectorShape2KiB>(sector_size, 2 * sector_count, sector_count, true, version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_two_partitions_matching_4kib_sub_8_2() -> Result<()> {
    let sector_size = SECTOR_SIZE_4_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape4KiB>(
            sector_size,
            2 * sector_count,
            sector_count,
            false,
            version,
        )?;
        window_post::<SectorShape4KiB>(sector_size, 2 * sector_count, sector_count, true, version)?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_two_partitions_matching_16kib_sub_8_8() -> Result<()> {
    let sector_size = SECTOR_SIZE_16_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape16KiB>(
            sector_size,
            2 * sector_count,
            sector_count,
            false,
            version,
        )?;
        window_post::<SectorShape16KiB>(
            sector_size,
            2 * sector_count,
            sector_count,
            true,
            version,
        )?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_two_partitions_matching_32kib_top_8_8_2() -> Result<()> {
    let sector_size = SECTOR_SIZE_32_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape32KiB>(
            sector_size,
            2 * sector_count,
            sector_count,
            false,
            version,
        )?;
        window_post::<SectorShape32KiB>(
            sector_size,
            2 * sector_count,
            sector_count,
            true,
            version,
        )?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_two_partitions_smaller_2kib_base_8() -> Result<()> {
    let sector_size = SECTOR_SIZE_2_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape2KiB>(
            sector_size,
            2 * sector_count - 1,
            sector_count,
            false,
            version,
        )?;
        window_post::<SectorShape2KiB>(
            sector_size,
            2 * sector_count - 1,
            sector_count,
            true,
            version,
        )?;
    }

    Ok(())
}

#[test]
#[ignore]
fn test_window_post_single_partition_matching_2kib_base_8() -> Result<()> {
    let sector_size = SECTOR_SIZE_2_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        window_post::<SectorShape2KiB>(sector_size, sector_count, sector_count, false, version)?;
        window_post::<SectorShape2KiB>(sector_size, sector_count, sector_count, true, version)?;
    }

    Ok(())
}

#[test]
fn test_window_post_partition_matching_2kib_base_8() -> Result<()> {
    let sector_size = SECTOR_SIZE_2_KIB;
    let sector_count = *WINDOW_POST_SECTOR_COUNT
        .read()
        .expect("WINDOW_POST_SECTOR_COUNT poisoned")
        .get(&sector_size)
        .expect("unknown sector size");

    let versions = vec![ApiVersion::V1_0_0, ApiVersion::V1_1_0, ApiVersion::V1_2_0];
    for version in versions {
        partition_window_post::<SectorShape2KiB>(
            sector_size,
            3, // Validate the scenarios of two partition
            sector_count,
            false,
            version,
        )?;
        partition_window_post::<SectorShape2KiB>(sector_size, 3, sector_count, true, version)?;
    }

    Ok(())
}

#[allow(clippy::iter_kv_map)]
fn partition_window_post<Tree: 'static + MerkleTreeTrait>(
    sector_size: u64,
    total_sector_count: usize,
    sector_count: usize,
    fake: bool,
    api_version: ApiVersion,
) -> Result<()> {
    use anyhow::anyhow;

    let mut rng = XorShiftRng::from_seed(TEST_SEED);

    let mut sectors = Vec::with_capacity(total_sector_count);
    let mut pub_replicas = BTreeMap::new();
    let mut priv_replicas = BTreeMap::new();

    let prover_fr: <Tree::Hasher as Hasher>::Domain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    let porep_id = match api_version {
        ApiVersion::V1_0_0 => ARBITRARY_POREP_ID_V1_0_0,
        ApiVersion::V1_1_0 => ARBITRARY_POREP_ID_V1_1_0,
        ApiVersion::V1_2_0 => ARBITRARY_POREP_ID_V1_2_0,
    };

    for _ in 0..total_sector_count {
        let (sector_id, replica, comm_r, cache_dir) = if fake {
            create_fake_seal::<_, Tree>(&mut rng, sector_size, &porep_id, api_version)?
        } else {
            create_seal::<_, Tree>(
                &mut rng,
                sector_size,
                prover_id,
                true,
                &porep_id,
                api_version,
            )?
        };
        priv_replicas.insert(
            sector_id,
            PrivateReplicaInfo::new(replica.path().into(), comm_r, cache_dir.path().into())?,
        );
        pub_replicas.insert(sector_id, PublicReplicaInfo::new(comm_r)?);
        sectors.push((sector_id, replica, comm_r, cache_dir, prover_id));
    }
    assert_eq!(priv_replicas.len(), total_sector_count);
    assert_eq!(pub_replicas.len(), total_sector_count);
    assert_eq!(sectors.len(), total_sector_count);

    let random_fr: <Tree::Hasher as Hasher>::Domain = Fr::random(&mut rng).into();
    let mut randomness = [0u8; 32];
    randomness.copy_from_slice(AsRef::<[u8]>::as_ref(&random_fr));

    let config = PoStConfig {
        sector_size: sector_size.into(),
        sector_count,
        challenge_count: WINDOW_POST_CHALLENGE_COUNT,
        typ: PoStType::Window,
        priority: false,
        api_version,
    };

    let replica_sectors = priv_replicas
        .iter()
        .map(|(sector, _replica)| *sector)
        .collect::<Vec<SectorId>>();

    let challenges = generate_fallback_sector_challenges::<Tree>(
        &config,
        &randomness,
        &replica_sectors,
        prover_id,
    )?;

    let num_sectors_per_chunk = config.sector_count;
    let mut proofs = Vec::new();

    let partitions = get_num_partition_for_fallback_post(&config, replica_sectors.len());
    for partition_index in 0..partitions {
        let sector_ids = replica_sectors
            .chunks(num_sectors_per_chunk)
            .nth(partition_index)
            .ok_or_else(|| anyhow!("invalid number of sectors/partition index"))?;

        let mut partition_priv_replicas = BTreeMap::new();
        for id in sector_ids {
            let p_sector = match priv_replicas.get(id) {
                Some(v) => v,
                _ => {
                    continue;
                }
            };

            partition_priv_replicas.insert(*id, p_sector);
        }

        let mut vanilla_proofs = Vec::new();
        for (sector_id, sector) in partition_priv_replicas.iter() {
            let sector_challenges = &challenges[sector_id];
            let single_proof = generate_single_vanilla_proof::<Tree>(
                &config,
                *sector_id,
                sector,
                sector_challenges,
            )?;

            vanilla_proofs.push(single_proof);
        }

        let proof = generate_single_window_post_with_vanilla(
            &config,
            &randomness,
            prover_id,
            vanilla_proofs,
            partition_index,
        )?;

        proofs.push(proof);
    }

    let final_proof = merge_window_post_partition_proofs(proofs)?;
    let valid =
        verify_window_post::<Tree>(&config, &randomness, &pub_replicas, prover_id, &final_proof)?;
    assert!(valid, "proofs did not verify");

    Ok(())
}

/// Make all files recursively read-only/writeable, starting at the given directory/file.
fn set_readonly_flag(path: &Path, readonly: bool) {
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry.expect("couldn't get file");
        let metadata = entry.metadata().expect("couldn't get metadata");
        let mut permissions = metadata.permissions();
        permissions.set_readonly(readonly);
        std::fs::set_permissions(entry.path(), permissions)
            .expect("couldn't apply read-only permissions");
    }
}

#[allow(clippy::iter_kv_map)]
fn window_post<Tree: 'static + MerkleTreeTrait>(
    sector_size: u64,
    total_sector_count: usize,
    sector_count: usize,
    fake: bool,
    api_version: ApiVersion,
) -> Result<()> {
    let mut rng = XorShiftRng::from_seed(TEST_SEED);

    let mut sectors = Vec::with_capacity(total_sector_count);
    let mut pub_replicas = BTreeMap::new();
    let mut priv_replicas = BTreeMap::new();
    let mut priv_faulty_replicas = BTreeMap::new();

    let prover_fr: <Tree::Hasher as Hasher>::Domain = Fr::random(&mut rng).into();
    let mut prover_id = [0u8; 32];
    prover_id.copy_from_slice(AsRef::<[u8]>::as_ref(&prover_fr));

    let porep_id = match api_version {
        ApiVersion::V1_0_0 => ARBITRARY_POREP_ID_V1_0_0,
        ApiVersion::V1_1_0 => ARBITRARY_POREP_ID_V1_1_0,
        ApiVersion::V1_2_0 => ARBITRARY_POREP_ID_V1_2_0,
    };

    for _ in 0..total_sector_count {
        let (sector_id, replica, comm_r, cache_dir) = if fake {
            create_fake_seal::<_, Tree>(&mut rng, sector_size, &porep_id, api_version)?
        } else {
            create_seal::<_, Tree>(
                &mut rng,
                sector_size,
                prover_id,
                true,
                &porep_id,
                api_version,
            )?
        };
        priv_replicas.insert(
            sector_id,
            PrivateReplicaInfo::new(replica.path().into(), comm_r, cache_dir.path().into())?,
        );

        // Create a bad replica (total failure) and add to
        // priv_faulty_replicas for checking later.
        //
        // Note: the file length makes it impossible to have any valid
        // proofs generated.  If we did something like
        // .set_len(metadata(replica.path())?.len() - 1)?
        // we could see a partial result (depending on sector shape).
        let bad_replica = NamedTempFile::new()?;
        bad_replica.as_file().set_len(1)?;
        priv_faulty_replicas.insert(
            sector_id,
            PrivateReplicaInfo::<Tree>::new(
                bad_replica.path().into(),
                comm_r,
                cache_dir.path().into(),
            )?,
        );
        bad_replica.keep()?;

        pub_replicas.insert(sector_id, PublicReplicaInfo::new(comm_r)?);
        sectors.push((sector_id, replica, comm_r, cache_dir, prover_id));
    }
    assert_eq!(priv_replicas.len(), total_sector_count);
    assert_eq!(pub_replicas.len(), total_sector_count);
    assert_eq!(sectors.len(), total_sector_count);

    let random_fr: <Tree::Hasher as Hasher>::Domain = Fr::random(&mut rng).into();
    let mut randomness = [0u8; 32];
    randomness.copy_from_slice(AsRef::<[u8]>::as_ref(&random_fr));

    let config = PoStConfig {
        sector_size: sector_size.into(),
        sector_count,
        challenge_count: WINDOW_POST_CHALLENGE_COUNT,
        typ: PoStType::Window,
        priority: false,
        api_version,
    };

    /////////////////////////////////////////////
    // The following methods of proof generation are functionally equivalent:
    // 1)
    let proof = generate_window_post::<Tree>(&config, &randomness, &priv_replicas, prover_id)?;

    let valid = verify_window_post::<Tree>(&config, &randomness, &pub_replicas, prover_id, &proof)?;
    assert!(valid, "proof did not verify");

    // 2)
    let replica_sectors = priv_replicas
        .iter()
        .map(|(sector, _replica)| *sector)
        .collect::<Vec<SectorId>>();

    let challenges = generate_fallback_sector_challenges::<Tree>(
        &config,
        &randomness,
        &replica_sectors,
        prover_id,
    )?;

    let mut vanilla_proofs = Vec::with_capacity(replica_sectors.len());

    // Make sure that files can be read-only for a window post.
    for (_, replica, _, cache_dir, _) in &sectors {
        set_readonly_flag(replica.path(), true);
        set_readonly_flag(cache_dir.path(), true);
    }

    for (sector_id, replica) in priv_replicas.iter() {
        let sector_challenges = &challenges[sector_id];
        let single_proof =
            generate_single_vanilla_proof::<Tree>(&config, *sector_id, replica, sector_challenges)?;

        vanilla_proofs.push(single_proof);
    }

    let proof =
        generate_window_post_with_vanilla::<Tree>(&config, &randomness, prover_id, vanilla_proofs)?;

    let valid = verify_window_post::<Tree>(&config, &randomness, &pub_replicas, prover_id, &proof)?;
    assert!(valid, "proof did not verify");
    /////////////////////////////////////////////

    // Lastly, let's ensure we're getting the faulty sectors.
    {
        let mut faulty_sectors = Vec::new();
        let proof =
            generate_window_post::<Tree>(&config, &randomness, &priv_faulty_replicas, prover_id);

        use storage_proofs_core::error::Error as FaultySectorError;
        match proof {
            Ok(proof) => {
                let valid = verify_window_post::<Tree>(
                    &config,
                    &randomness,
                    &pub_replicas,
                    prover_id,
                    &proof,
                )?;
                assert!(!valid, "proof made with faulty sectors verified");
            }
            Err(e) => match e.downcast::<FaultySectorError>() {
                Err(_) => panic!("failed to downcast to Error"),
                Ok(FaultySectorError::FaultySectors(sector_ids)) => {
                    info!("faulty_sectors detected properly: {:?}", sector_ids);
                    faulty_sectors.extend(sector_ids);
                }
                Ok(_) => panic!("PoSt failed to return FaultySectors error."),
            },
        };

        // This assertion is for the case of a total failure, not a
        // partial failure.
        assert_eq!(
            faulty_sectors.len(),
            priv_faulty_replicas.len(),
            "faulty sector detection failure"
        );

        priv_faulty_replicas
            .iter()
            .for_each(|(sector_id, faulty_replica)| {
                // Ensure we have a record of the faulty sector
                assert!(
                    faulty_sectors.contains(sector_id),
                    "faulty sector not reported"
                );
                // Delete temporary faulty_replica files.
                remove_file(faulty_replica.replica_path()).expect("failed to remove faulty_replica")
            });
    }

    // Make files writeable again, so that the temporary directory can be removed.
    for (_, replica, _, cache_dir, _) in &sectors {
        set_readonly_flag(replica.path(), false);
        set_readonly_flag(cache_dir.path(), false);
    }

    Ok(())
}

fn generate_piece_file(sector_size: u64) -> Result<(NamedTempFile, Vec<u8>)> {
    let number_of_bytes_in_piece = UnpaddedBytesAmount::from(PaddedBytesAmount(sector_size));

    let piece_bytes: Vec<u8> = (0..number_of_bytes_in_piece.0)
        .map(|_| random::<u8>())
        .collect();

    let mut piece_file = NamedTempFile::new()?;
    piece_file.write_all(&piece_bytes)?;
    piece_file.as_file_mut().sync_all()?;
    piece_file.as_file_mut().rewind()?;

    Ok((piece_file, piece_bytes))
}

fn porep_config(sector_size: u64, porep_id: [u8; 32], api_version: ApiVersion) -> PoRepConfig {
    PoRepConfig::new_groth16(sector_size, porep_id, api_version)
}

fn run_seal_pre_commit_phase1<Tree: 'static + MerkleTreeTrait>(
    config: &PoRepConfig,
    prover_id: ProverId,
    sector_id: SectorId,
    ticket: [u8; 32],
    cache_dir: &TempDir,
    mut piece_file: &mut NamedTempFile,
    sealed_sector_file: &NamedTempFile,
) -> Result<(Vec<PieceInfo>, SealPreCommitPhase1Output<Tree>)> {
    let number_of_bytes_in_piece = config.unpadded_bytes_amount();

    let piece_info = generate_piece_commitment(piece_file.as_file_mut(), number_of_bytes_in_piece)?;
    piece_file.as_file_mut().rewind()?;

    let mut staged_sector_file = NamedTempFile::new()?;
    add_piece(
        &mut piece_file,
        &mut staged_sector_file,
        number_of_bytes_in_piece,
        &[],
    )?;

    let piece_infos = vec![piece_info];

    let phase1_output = seal_pre_commit_phase1::<_, _, _, Tree>(
        config,
        cache_dir.path(),
        staged_sector_file.path(),
        sealed_sector_file.path(),
        prover_id,
        sector_id,
        ticket,
        &piece_infos,
    )?;

    validate_cache_for_precommit_phase2(
        cache_dir.path(),
        staged_sector_file.path(),
        &phase1_output,
    )?;

    Ok((piece_infos, phase1_output))
}

#[allow(clippy::too_many_arguments)]
fn generate_proof<Tree: 'static + MerkleTreeTrait>(
    config: &PoRepConfig,
    cache_dir_path: &Path,
    sealed_sector_file: &NamedTempFile,
    prover_id: ProverId,
    sector_id: SectorId,
    ticket: [u8; 32],
    seed: [u8; 32],
    pre_commit_output: &SealPreCommitOutput,
    piece_infos: &[PieceInfo],
) -> Result<(SealCommitOutput, Vec<Vec<Fr>>, [u8; 32], [u8; 32])> {
    let phase1_output = seal_commit_phase1::<_, Tree>(
        config,
        cache_dir_path,
        sealed_sector_file.path(),
        prover_id,
        sector_id,
        ticket,
        seed,
        pre_commit_output.clone(),
        piece_infos,
    )?;

    clear_cache::<Tree>(cache_dir_path)?;

    ensure!(
        seed == phase1_output.seed,
        "seed and phase1 output seed do not match"
    );
    ensure!(
        ticket == phase1_output.ticket,
        "seed and phase1 output ticket do not match"
    );

    let comm_r = phase1_output.comm_r;
    let inputs = get_seal_inputs::<Tree>(
        config,
        phase1_output.comm_r,
        phase1_output.comm_d,
        prover_id,
        sector_id,
        phase1_output.ticket,
        phase1_output.seed,
    )?;
    let result = seal_commit_phase2(config, phase1_output, prover_id, sector_id)?;

    Ok((result, inputs, seed, comm_r))
}

#[allow(clippy::too_many_arguments)]
fn unseal<Tree: 'static + MerkleTreeTrait>(
    config: &PoRepConfig,
    cache_dir_path: &Path,
    sealed_sector_file: &NamedTempFile,
    prover_id: ProverId,
    sector_id: SectorId,
    ticket: [u8; 32],
    seed: [u8; 32],
    pre_commit_output: &SealPreCommitOutput,
    piece_infos: &[PieceInfo],
    piece_bytes: &[u8],
    commit_output: &SealCommitOutput,
) -> Result<()> {
    let comm_d = pre_commit_output.comm_d;
    let comm_r = pre_commit_output.comm_r;

    let mut unseal_file = NamedTempFile::new()?;
    let _ = unseal_range::<_, _, _, Tree>(
        config,
        cache_dir_path,
        sealed_sector_file,
        &unseal_file,
        prover_id,
        sector_id,
        comm_d,
        ticket,
        UnpaddedByteIndex(508),
        UnpaddedBytesAmount(508),
    )?;

    unseal_file.rewind()?;

    let mut contents = vec![];
    assert!(
        unseal_file.read_to_end(&mut contents).is_ok(),
        "failed to populate buffer with unsealed bytes"
    );
    assert_eq!(contents.len(), 508);
    assert_eq!(&piece_bytes[508..508 + 508], &contents[..]);

    let computed_comm_d = compute_comm_d(config.sector_size, piece_infos)?;

    assert_eq!(
        comm_d, computed_comm_d,
        "Computed and expected comm_d don't match."
    );

    let verified = verify_seal::<Tree>(
        config,
        comm_r,
        comm_d,
        prover_id,
        sector_id,
        ticket,
        seed,
        &commit_output.proof,
    )?;
    assert!(verified, "failed to verify valid seal");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn proof_and_unseal<Tree: 'static + MerkleTreeTrait>(
    config: &PoRepConfig,
    cache_dir_path: &Path,
    sealed_sector_file: &NamedTempFile,
    prover_id: ProverId,
    sector_id: SectorId,
    ticket: [u8; 32],
    seed: [u8; 32],
    pre_commit_output: SealPreCommitOutput,
    piece_infos: &[PieceInfo],
    piece_bytes: &[u8],
) -> Result<()> {
    let (commit_output, _commit_inputs, _seed, _comm_r) = generate_proof::<Tree>(
        config,
        cache_dir_path,
        sealed_sector_file,
        prover_id,
        sector_id,
        ticket,
        seed,
        &pre_commit_output,
        piece_infos,
    )?;

    unseal::<Tree>(
        config,
        cache_dir_path,
        sealed_sector_file,
        prover_id,
        sector_id,
        ticket,
        seed,
        &pre_commit_output,
        piece_infos,
        piece_bytes,
        &commit_output,
    )
}

fn create_seal<R: Rng, Tree: 'static + MerkleTreeTrait>(
    rng: &mut R,
    sector_size: u64,
    prover_id: ProverId,
    skip_proof: bool,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) -> Result<(SectorId, NamedTempFile, Commitment, TempDir)> {
    fil_logger::maybe_init();

    let (mut piece_file, piece_bytes) = generate_piece_file(sector_size)?;
    let sealed_sector_file = NamedTempFile::new()?;
    let cache_dir = tempdir().expect("failed to create temp dir");

    let config = porep_config(sector_size, *porep_id, api_version);
    let ticket = rng.gen();
    let seed = rng.gen();
    let sector_id = rng.gen::<u64>().into();

    let (piece_infos, phase1_output) = run_seal_pre_commit_phase1::<Tree>(
        &config,
        prover_id,
        sector_id,
        ticket,
        &cache_dir,
        &mut piece_file,
        &sealed_sector_file,
    )?;

    let pre_commit_output = seal_pre_commit_phase2(
        &config,
        phase1_output,
        cache_dir.path(),
        sealed_sector_file.path(),
    )?;

    let comm_r = pre_commit_output.comm_r;

    validate_cache_for_commit::<_, _, Tree>(cache_dir.path(), sealed_sector_file.path())?;

    if skip_proof {
        clear_cache::<Tree>(cache_dir.path())?;
    } else {
        proof_and_unseal::<Tree>(
            &config,
            cache_dir.path(),
            &sealed_sector_file,
            prover_id,
            sector_id,
            ticket,
            seed,
            pre_commit_output,
            &piece_infos,
            &piece_bytes,
        )
        .expect("failed to proof_and_unseal");
    }

    Ok((sector_id, sealed_sector_file, comm_r, cache_dir))
}

fn create_seal_for_aggregation<R: Rng, Tree: 'static + MerkleTreeTrait>(
    rng: &mut R,
    sector_size: u64,
    prover_id: ProverId,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) -> Result<(SealCommitOutput, Vec<Vec<Fr>>, [u8; 32], [u8; 32])> {
    fil_logger::maybe_init();

    let (mut piece_file, _piece_bytes) = generate_piece_file(sector_size)?;
    let sealed_sector_file = NamedTempFile::new()?;
    let cache_dir = tempfile::tempdir().expect("failed to create temp dir");

    let config = porep_config(sector_size, *porep_id, api_version);
    let ticket = rng.gen();
    let seed = rng.gen();
    let sector_id = rng.gen::<u64>().into();

    let (piece_infos, phase1_output) = run_seal_pre_commit_phase1::<Tree>(
        &config,
        prover_id,
        sector_id,
        ticket,
        &cache_dir,
        &mut piece_file,
        &sealed_sector_file,
    )?;

    let pre_commit_output = seal_pre_commit_phase2(
        &config,
        phase1_output,
        cache_dir.path(),
        sealed_sector_file.path(),
    )?;

    validate_cache_for_commit::<_, _, Tree>(cache_dir.path(), sealed_sector_file.path())?;

    generate_proof::<Tree>(
        &config,
        cache_dir.path(),
        &sealed_sector_file,
        prover_id,
        sector_id,
        ticket,
        seed,
        &pre_commit_output,
        &piece_infos,
    )
}

fn compare_elements(path1: &Path, path2: &Path) -> Result<(), Error> {
    info!("Comparing elements between {:?} and {:?}", path1, path2);
    let f_data1 = OpenOptions::new()
        .read(true)
        .open(path1)
        .with_context(|| format!("could not open path={:?}", path1))?;
    let data1 = unsafe {
        MmapOptions::new()
            .map(&f_data1)
            .with_context(|| format!("could not mmap path={:?}", path1))
    }?;
    let f_data2 = OpenOptions::new()
        .read(true)
        .open(path2)
        .with_context(|| format!("could not open path={:?}", path2))?;
    let data2 = unsafe {
        MmapOptions::new()
            .map(&f_data2)
            .with_context(|| format!("could not mmap path={:?}", path2))
    }?;
    let fr_size = std::mem::size_of::<Fr>();
    let end = metadata(path1)?.len();
    ensure!(metadata(path2)?.len() == end, "File sizes must match");

    for i in (0..end).step_by(fr_size) {
        let index = i as usize;
        let fr1 = bytes_into_fr(&data1[index..index + fr_size])?;
        let fr2 = bytes_into_fr(&data2[index..index + fr_size])?;
        ensure!(fr1 == fr2, "Data mismatch when comparing elements");
    }
    info!("Match found for {:?} and {:?}", path1, path2);

    Ok(())
}

fn create_seal_for_upgrade<R: Rng, Tree: 'static + MerkleTreeTrait<Hasher = TreeRHasher>>(
    rng: &mut R,
    sector_size: u64,
    prover_id: ProverId,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) -> Result<(SectorId, NamedTempFile, Commitment, TempDir)> {
    fil_logger::maybe_init();

    let (mut piece_file, _piece_bytes) = generate_piece_file(sector_size)?;
    let sealed_sector_file = NamedTempFile::new()?;
    let cache_dir = tempdir().expect("failed to create temp dir");

    let porep_config = porep_config(sector_size, *porep_id, api_version);
    let config = SectorUpdateConfig::from_porep_config(&porep_config);
    let ticket = rng.gen();
    let sector_id = rng.gen::<u64>().into();

    let (_piece_infos, phase1_output) = run_seal_pre_commit_phase1::<Tree>(
        &porep_config,
        prover_id,
        sector_id,
        ticket,
        &cache_dir,
        &mut piece_file,
        &sealed_sector_file,
    )?;

    let pre_commit_output = seal_pre_commit_phase2(
        &porep_config,
        phase1_output,
        cache_dir.path(),
        sealed_sector_file.path(),
    )?;
    let comm_r = pre_commit_output.comm_r;

    validate_cache_for_commit::<_, _, Tree>(cache_dir.path(), sealed_sector_file.path())?;

    // Upgrade the cc sector here.
    let new_sealed_sector_file = NamedTempFile::new()?;
    let new_cache_dir = tempdir().expect("failed to create temp dir");

    // create and generate some random data in staged_data_file.
    let (mut new_piece_file, _new_piece_bytes) = generate_piece_file(sector_size)?;
    let number_of_bytes_in_piece = porep_config.unpadded_bytes_amount();

    let new_piece_info =
        generate_piece_commitment(new_piece_file.as_file_mut(), number_of_bytes_in_piece)?;
    new_piece_file.as_file_mut().rewind()?;

    let mut new_staged_sector_file = NamedTempFile::new()?;
    add_piece(
        &mut new_piece_file,
        &mut new_staged_sector_file,
        number_of_bytes_in_piece,
        &[],
    )?;

    let new_piece_infos = vec![new_piece_info];

    // New replica (new_sealed_sector_file) is currently 0 bytes --
    // set a length here to ensure proper mmap later.  Lotus will
    // already be passing in a destination path of the proper size in
    // the future, so this is a test specific work-around.
    let new_replica_target_len = metadata(&sealed_sector_file)?.len();
    let f_sealed_sector = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(new_sealed_sector_file.path())
        .with_context(|| format!("could not open path={:?}", new_sealed_sector_file.path()))?;
    f_sealed_sector.set_len(new_replica_target_len)?;

    let encoded = encode_into::<Tree>(
        &porep_config,
        new_sealed_sector_file.path(),
        new_cache_dir.path(),
        sealed_sector_file.path(),
        cache_dir.path(),
        new_staged_sector_file.path(),
        &new_piece_infos,
    )?;

    // Generate a single partition proof
    let partition_proof = generate_single_partition_proof::<Tree>(
        config,
        0, // first partition
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
        sealed_sector_file.path(), /* sector key file */
        cache_dir.path(),          /* sector key path needed for p_aux and t_aux */
        new_sealed_sector_file.path(),
        new_cache_dir.path(),
    )?;

    // Verify the single partition proof
    let proof_is_valid = verify_single_partition_proof::<Tree>(
        config,
        0, // first partition
        partition_proof,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
    )?;
    ensure!(proof_is_valid, "Partition proof (single) failed to verify");

    // Generate all partition proofs
    let partition_proofs = generate_partition_proofs::<Tree>(
        config,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
        sealed_sector_file.path(), /* sector key file */
        cache_dir.path(),          /* sector key path needed for p_aux and t_aux */
        new_sealed_sector_file.path(),
        new_cache_dir.path(),
    )?;

    // Verify all partition proofs
    let proofs_are_valid = verify_partition_proofs::<Tree>(
        config,
        &partition_proofs,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
    )?;
    ensure!(proofs_are_valid, "Partition proofs failed to verify");

    let proof = generate_empty_sector_update_proof_with_vanilla::<Tree>(
        &porep_config,
        partition_proofs,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
    )?;
    let valid = verify_empty_sector_update_proof::<Tree>(
        &porep_config,
        &proof.0,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
    )?;
    ensure!(valid, "Compound proof failed to verify");

    let proof = generate_empty_sector_update_proof::<Tree>(
        &porep_config,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
        sealed_sector_file.path(), /* sector key file */
        cache_dir.path(),          /* sector key path needed for p_aux and t_aux */
        new_sealed_sector_file.path(),
        new_cache_dir.path(),
    )?;
    let valid = verify_empty_sector_update_proof::<Tree>(
        &porep_config,
        &proof.0,
        comm_r,
        encoded.comm_r_new,
        encoded.comm_d_new,
    )?;
    ensure!(valid, "Compound proof failed to verify");

    let decoded_sector_file = NamedTempFile::new()?;
    // New replica (new_sealed_sector_file) is currently 0 bytes --
    // set a length here to ensure proper mmap later.  Lotus will
    // already be passing in a destination path of the proper size in
    // the future, so this is a test specific work-around.
    let decoded_sector_target_len = metadata(&sealed_sector_file)?.len();
    let f_decoded_sector = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(decoded_sector_file.path())
        .with_context(|| format!("could not open path={:?}", decoded_sector_file.path()))?;
    f_decoded_sector.set_len(decoded_sector_target_len)?;

    decode_from::<Tree>(
        config,
        decoded_sector_file.path(),
        new_sealed_sector_file.path(),
        sealed_sector_file.path(),
        cache_dir.path(), /* sector key path needed for p_aux (for comm_c/comm_r_last) */
        encoded.comm_d_new,
    )?;
    // When the data is decoded, it MUST match the original new staged data.
    compare_elements(decoded_sector_file.path(), new_staged_sector_file.path())?;

    decoded_sector_file.close()?;

    // Remove Data here
    let remove_encoded_file = NamedTempFile::new()?;
    let remove_encoded_cache_dir = tempdir().expect("failed to create temp dir");
    // New replica (new_sealed_sector_file) is currently 0 bytes --
    // set a length here to ensure proper mmap later.  Lotus will
    // already be passing in a destination path of the proper size in
    // the future, so this is a test specific work-around.
    let remove_encoded_target_len = metadata(&sealed_sector_file)?.len();
    let f_remove_encoded = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(remove_encoded_file.path())
        .with_context(|| format!("could not open path={:?}", remove_encoded_file.path()))?;
    f_remove_encoded.set_len(remove_encoded_target_len)?;

    // Note: we pass cache_dir to the remove, which is the original
    // dir where the data was sealed (for p_aux/t_aux).
    remove_encoded_data::<Tree>(
        config,
        remove_encoded_file.path(),
        remove_encoded_cache_dir.path(),
        new_sealed_sector_file.path(),
        cache_dir.path(),
        new_staged_sector_file.path(),
        encoded.comm_d_new,
    )?;
    // When the data is removed, it MUST match the original sealed data.
    compare_elements(remove_encoded_file.path(), sealed_sector_file.path())?;

    remove_encoded_file.close()?;

    clear_cache::<Tree>(cache_dir.path())?;
    clear_cache::<Tree>(new_cache_dir.path())?;

    Ok((sector_id, sealed_sector_file, comm_r, cache_dir))
}

fn create_fake_seal<R: rand::Rng, Tree: 'static + MerkleTreeTrait>(
    mut rng: &mut R,
    sector_size: u64,
    porep_id: &[u8; 32],
    api_version: ApiVersion,
) -> Result<(SectorId, NamedTempFile, Commitment, TempDir)> {
    fil_logger::maybe_init();

    let sealed_sector_file = NamedTempFile::new()?;

    let config = porep_config(sector_size, *porep_id, api_version);

    let cache_dir = tempdir().unwrap();

    let sector_id = rng.gen::<u64>().into();

    let comm_r = fauxrep_aux::<_, _, _, Tree>(
        &mut rng,
        &config,
        cache_dir.path(),
        sealed_sector_file.path(),
    )?;

    Ok((sector_id, sealed_sector_file, comm_r, cache_dir))
}

#[test]
fn test_aggregate_proof_encode_decode() -> Result<()> {
    // This byte vector is a natively serialized aggregate proof generated from the
    // 'test_seal_proof_aggregation_257_2kib_porep_id_v1_1_base_8' test.
    let aggregate_proof_bytes = std::include_bytes!("./aggregate_proof_bytes");
    let expected_aggregate_proof_len = 29_044;

    // Re-construct the aggregate proof from the bytes, using the native deserialization method.
    let aggregate_proof: groth16::aggregate::AggregateProof<Bls12> =
        groth16::aggregate::AggregateProof::read(std::io::Cursor::new(&aggregate_proof_bytes))?;
    let aggregate_proof_count = aggregate_proof.tmipp.gipa.nproofs as usize;
    let expected_aggregate_proof_count = 512;

    assert_eq!(aggregate_proof_count, expected_aggregate_proof_count);

    // Re-serialize the proof to ensure a round-trip match.
    let mut aggregate_proof_bytes2 = Vec::new();
    aggregate_proof.write(&mut aggregate_proof_bytes2)?;

    assert_eq!(aggregate_proof_bytes.len(), expected_aggregate_proof_len);
    assert_eq!(aggregate_proof_bytes.len(), aggregate_proof_bytes2.len());
    assert_eq!(aggregate_proof_bytes, aggregate_proof_bytes2.as_slice());

    // Note: the native serialization format is more compact than bincode serialization, so assert that here.
    let bincode_serialized_proof = serialize(&aggregate_proof)?;
    let expected_bincode_serialized_proof_len = 56_436;

    assert!(aggregate_proof_bytes2.len() < bincode_serialized_proof.len());
    assert_eq!(
        bincode_serialized_proof.len(),
        expected_bincode_serialized_proof_len
    );

    Ok(())
}
