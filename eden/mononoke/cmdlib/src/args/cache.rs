/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use blobrepo_factory::Caching;
use clap::{App, Arg, ArgMatches};
use fbinit::FacebookInit;
use once_cell::sync::{Lazy, OnceCell};

use crate::args::MononokeMatches;

const CACHE_SIZE_GB: &str = "cache-size-gb";
const USE_TUPPERWARE_SHRINKER: &str = "use-tupperware-shrinker";
const MAX_PROCESS_SIZE: &str = "max-process-size";
const MIN_PROCESS_SIZE: &str = "min-process-size";
const SKIP_CACHING: &str = "skip-caching";
const CACHELIB_ONLY_BLOBSTORE: &str = "cachelib-only-blobstore";
const CACHELIB_SHARDS: &str = "cachelib-shards";

const PHASES_CACHE_SIZE: &str = "phases-cache-size";
const BUCKETS_POWER: &str = "buckets-power";

const ONE_GIB: usize = 1073741824; // 2^30 aka 1GiB

const CACHE_ARGS: &[(&str, &str)] = &[
    ("blob-cache-size", "override size of the blob cache"),
    (
        "presence-cache-size",
        "override size of the blob presence cache",
    ),
    (
        "changesets-cache-size",
        "override size of the changesets cache",
    ),
    (
        "filenodes-cache-size",
        "override size of the filenodes cache (individual filenodes)",
    ),
    (
        "filenodes-history-cache-size",
        "override size of the filenodes history cache (entire batches of history for a node)",
    ),
    (
        "idmapping-cache-size",
        "override size of the bonsai/hg mapping cache",
    ),
    (PHASES_CACHE_SIZE, "override size of the phases cache"),
    (
        BUCKETS_POWER,
        "override the bucket power for cachelib's hashtable",
    ),
];

pub(crate) fn add_cachelib_args<'a, 'b>(
    app: App<'a, 'b>,
    hide_advanced_args: bool,
    defaults: CachelibSettings,
) -> App<'a, 'b> {
    let cache_args: Vec<_> = CACHE_ARGS
        .iter()
        .map(|(flag, help)| {
            // XXX figure out a way to get default values in here -- note that .default_value
            // takes a &'a str, so we may need to have MononokeAppBuilder own it or similar.
            Arg::with_name(flag)
                .long(flag)
                .value_name("SIZE")
                .hidden(hide_advanced_args)
                .help(help)
        })
        .collect();

    // Computed default_value and help strings with lifetime 'b is problematic, so use once_cell
    static MIN_PROCESS_SIZE_HELP: Lazy<String> = Lazy::new(|| {
        format!(
            "process size at which cachelib will grow back to {} in GiB",
            CACHE_SIZE_GB
        )
    });

    static CACHE_SIZE_GB_DEFAULT: OnceCell<String> = OnceCell::new();

    app.arg(
        Arg::with_name(CACHE_SIZE_GB)
            .long(CACHE_SIZE_GB)
            .takes_value(true)
            .value_name("SIZE")
            .default_value(
                CACHE_SIZE_GB_DEFAULT.get_or_init(|| (defaults.cache_size / ONE_GIB).to_string()),
            )
            .help("size of the cachelib cache, in GiB"),
    )
    .arg(
        Arg::with_name(USE_TUPPERWARE_SHRINKER)
            .long(USE_TUPPERWARE_SHRINKER)
            .help("Use the Tupperware-aware cache shrinker to avoid OOM"),
    )
    .arg(
        Arg::with_name(MAX_PROCESS_SIZE)
            .long(MAX_PROCESS_SIZE)
            .takes_value(true)
            .value_name("SIZE")
            .help("process size at which cachelib will shrink, in GiB"),
    )
    .arg(
        Arg::with_name(MIN_PROCESS_SIZE)
            .long(MIN_PROCESS_SIZE)
            .takes_value(true)
            .value_name("SIZE")
            .help(&MIN_PROCESS_SIZE_HELP),
    )
    .arg(
        Arg::with_name(SKIP_CACHING)
            .long(SKIP_CACHING)
            .help("do not init cachelib and disable caches (useful for tests)"),
    )
    .arg(
        Arg::with_name(CACHELIB_ONLY_BLOBSTORE)
            .long(CACHELIB_ONLY_BLOBSTORE)
            .help("do not init memcache for blobstore"),
    )
    .arg(
        Arg::with_name(CACHELIB_SHARDS)
            .long(CACHELIB_SHARDS)
            .takes_value(true)
            .help("number of shards to control concurrent access to a blobstore behind cachelib"),
    )
    .args(&cache_args)
}

pub(crate) fn parse_cachelib_shards<'a>(matches: &ArgMatches<'a>) -> usize {
    match matches.value_of(CACHELIB_SHARDS) {
        Some(v) => v.parse().unwrap(),
        None => 0,
    }
}

pub(crate) fn parse_caching<'a>(matches: &ArgMatches<'a>) -> Caching {
    if matches.is_present(SKIP_CACHING) {
        Caching::Disabled
    } else if matches.is_present(CACHELIB_ONLY_BLOBSTORE) {
        Caching::CachelibOnlyBlobstore(parse_cachelib_shards(matches))
    } else {
        Caching::Enabled(parse_cachelib_shards(matches))
    }
}

/// Usual entry point where binary is happy with CachelibSettings::default()
pub fn init_cachelib<'a>(fb: FacebookInit, matches: &'a MononokeMatches<'a>) -> Caching {
    parse_and_init_cachelib(
        fb,
        matches.as_ref(),
        matches.app_data.cachelib_settings.clone(),
    )
}

/// Provide a way for binaries to specify if they have different default cachelib settings
pub(crate) fn parse_and_init_cachelib<'a>(
    fb: FacebookInit,
    matches: &ArgMatches<'a>,
    mut settings: CachelibSettings,
) -> Caching {
    let caching = parse_caching(matches);

    match caching {
        Caching::Enabled(..) | Caching::CachelibOnlyBlobstore(..) => {
            if let Some(cache_size) = matches.value_of(CACHE_SIZE_GB) {
                settings.cache_size =
                    (cache_size.parse::<f64>().unwrap() * ONE_GIB as f64) as usize;
            }
            if let Some(max_process_size) = matches.value_of(MAX_PROCESS_SIZE) {
                settings.max_process_size_gib = Some(max_process_size.parse().unwrap());
            }
            if let Some(min_process_size) = matches.value_of(MIN_PROCESS_SIZE) {
                settings.min_process_size_gib = Some(min_process_size.parse().unwrap());
            }
            settings.use_tupperware_shrinker = matches.is_present(USE_TUPPERWARE_SHRINKER);
            if let Some(presence_cache_size) = matches.value_of("presence-cache-size") {
                settings.presence_cache_size = Some(presence_cache_size.parse().unwrap());
            }
            if let Some(changesets_cache_size) = matches.value_of("changesets-cache-size") {
                settings.changesets_cache_size = Some(changesets_cache_size.parse().unwrap());
            }
            if let Some(filenodes_cache_size) = matches.value_of("filenodes-cache-size") {
                settings.filenodes_cache_size = Some(filenodes_cache_size.parse().unwrap());
            }
            if let Some(filenodes_history_cache_size) =
                matches.value_of("filenodes-history-cache-size")
            {
                settings.filenodes_history_cache_size =
                    Some(filenodes_history_cache_size.parse().unwrap());
            }
            if let Some(idmapping_cache_size) = matches.value_of("idmapping-cache-size") {
                settings.idmapping_cache_size = Some(idmapping_cache_size.parse().unwrap());
            }
            if let Some(blob_cache_size) = matches.value_of("blob-cache-size") {
                settings.blob_cache_size = Some(blob_cache_size.parse().unwrap());
            }
            if let Some(phases_cache_size) = matches.value_of(PHASES_CACHE_SIZE) {
                settings.phases_cache_size = Some(phases_cache_size.parse().unwrap());
            }
            if let Some(buckets_power) = matches.value_of(BUCKETS_POWER) {
                settings.buckets_power = Some(buckets_power.parse().unwrap());
            }

            #[cfg(not(fbcode_build))]
            {
                let _ = fb;
                unimplemented!("Initialization of cachelib works only for fbcode builds")
            }
            #[cfg(fbcode_build)]
            {
                super::facebook::init_cachelib_from_settings(fb, settings).unwrap();
            }
        }
        Caching::Disabled => {
            // No-op
        }
    };

    caching
}

#[derive(Clone)]
pub struct CachelibSettings {
    pub cache_size: usize,
    pub max_process_size_gib: Option<u32>,
    pub min_process_size_gib: Option<u32>,
    pub buckets_power: Option<u32>,
    pub use_tupperware_shrinker: bool,
    pub presence_cache_size: Option<usize>,
    pub changesets_cache_size: Option<usize>,
    pub filenodes_cache_size: Option<usize>,
    pub filenodes_history_cache_size: Option<usize>,
    pub idmapping_cache_size: Option<usize>,
    pub blob_cache_size: Option<usize>,
    pub phases_cache_size: Option<usize>,
    pub expected_item_size_bytes: Option<usize>,
}

impl Default for CachelibSettings {
    fn default() -> Self {
        Self {
            cache_size: 20 * ONE_GIB,
            max_process_size_gib: None,
            min_process_size_gib: None,
            buckets_power: None,
            use_tupperware_shrinker: false,
            presence_cache_size: None,
            changesets_cache_size: None,
            filenodes_cache_size: None,
            filenodes_history_cache_size: None,
            idmapping_cache_size: None,
            blob_cache_size: None,
            phases_cache_size: None,
            expected_item_size_bytes: None,
        }
    }
}
