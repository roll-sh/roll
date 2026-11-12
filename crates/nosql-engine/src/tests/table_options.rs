//! Table option validation probed on both references (`compaction` sub-options per strategy,
//! `compression` with `chunk_length_in_kb`, `speculative_retry`, `caching`, index intervals,
//! `bloom_filter_fp_chance`, …): each `ALTER TABLE` on a fresh `ks.t (a int PRIMARY KEY, b
//! int)`, in order, with each reference's outcome (code and message). ScyllaDB answers
//! `'dog'` / `'99p'` speculative retries with a server error ("sstring out of range").

use super::*;

#[derive(Clone, Copy, Debug)]
enum O {
    Ok,
    Err(i32, &'static str),
}

const CASES: &[(&str, O, O)] = &[
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_threshold' : -0.4 }", O::Err(0x2300, "tombstone_threshold value (-0.4) must be between 0.0 and 1.0"), O::Err(0x2300, "tombstone_threshold must be greater than 0, but was -0.400000")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'tombstone_threshold' : 5.5 }", O::Err(0x2300, "tombstone_threshold value (5.5) must be between 0.0 and 1.0"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'tombstone_compaction_interval' : -7 }", O::Err(0x2300, "tombstone_compaction_interval value (-7000ms) must be positive"), O::Err(0x2300, "tombstone_compaction_interval must be greater than 0, but was -7")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'unchecked_tombstone_compaction' : 'maybe' }", O::Err(0x2300, "unchecked_tombstone_compaction value (maybe) must be \"true\" or \"false\""), O::Err(0x2300, "'unchecked_tombstone_compaction' should be either 'true' or 'false', not 'maybe'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'enabled' : 'certainly' }", O::Err(0x2300, "enabled value (certainly) must be \"true\" or \"false\""), O::Err(0x2300, "enabled should either be 'true' or 'false', not certainly")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_sstable_size' : -1 }", O::Err(0x2300, "min_sstable_size value (-1) must be non negative"), O::Err(0x2300, "min_sstable_size must be non negative: -1")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_low' : 0.0 }", O::Err(0x2300, "bucket_low value (0) must be between 0.0 and 1.0"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_low' : 1.3 }", O::Err(0x2300, "bucket_low value (1.3) must be between 0.0 and 1.0"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_high' : 0.7 }", O::Err(0x2300, "bucket_high value (0.7) must be greater than 1.0"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'cold_reads_to_omit' : -8.1 }", O::Err(0x2300, "cold_reads_to_omit value (-8.1) must be between 0.0 and 1.0"), O::Err(0x2300, "Properties specified [cold_reads_to_omit] are not understood by SizeTieredCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'cold_reads_to_omit' : 3.5 }", O::Err(0x2300, "cold_reads_to_omit value (3.5) must be between 0.0 and 1.0"), O::Err(0x2300, "Properties specified [cold_reads_to_omit] are not understood by SizeTieredCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_threshold' : 1 }", O::Err(0x2300, "min_threshold value (1) must be bigger or equal to 2"), O::Err(0x2300, "Min compaction threshold cannot be less than 2 (got 1)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_unit' : 'SECONDS' }", O::Err(0x2300, "Invalid window unit SECONDS for compaction_window_unit"), O::Err(0x2300, "SECONDS is not valid for compaction_window_unit")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_size' : -8 }", O::Err(0x2300, "compaction_window_size value (-8) must be greater than 1"), O::Err(0x2300, "-8 must be greater than 1 for compaction_window_size")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'timestamp_resolution' : 'SECONDS' }", O::Err(0x2300, "Invalid timestamp resolution SECONDS for timestamp_resolution"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'enable_optimized_twcs_queries' : 'no' }", O::Err(0x2300, "enable_optimized_twcs_queries value (no) must be \"true\" or \"false\""), O::Err(0x2300, "Properties specified [enable_optimized_twcs_queries] are not understood by TimeWindowCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'max_threshold' : 1 }", O::Err(0x2300, "max_threshold value (1) must be bigger or equal to 2"), O::Err(0x2300, "Min compaction threshold (got 4) cannot be greater than max compaction threshold (got 1)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'sstable_size_in_mb' : -5 }", O::Err(0x2300, "sstable_size_in_mb value (-5) must be positive"), O::Err(0x2300, "sstable_size_in_mb must be larger than 0, but was -5")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'sstable_size_in_mb' : 2147483648 }", O::Err(0x2000, "Invalid integer value 2147483648 for 'sstable_size_in_mb'"), O::Err(0x2300, "2147483648 is not a parsable int (base10) for sstable_size_in_mb")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'min_sstable_size' : -1 }", O::Err(0x2300, "min_sstable_size value (-1) must be non negative"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'bucket_low' : 0.0 }", O::Err(0x2300, "bucket_low value (0) must be between 0.0 and 1.0"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'bucket_high' : 0.7 }", O::Err(0x2300, "bucket_high value (0.7) must be greater than 1.0"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'space_amplification_goal' : 2.2 }", O::Err(0x2300, "space_amplification_goal value (2.2) must be greater than 1.0 and less than or equal to 2.0"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'space_amplification_goal' : 1.0 }", O::Err(0x2300, "space_amplification_goal value (1) must be greater than 1.0 and less than or equal to 2.0"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'min_threshold' : 1 }", O::Err(0x2300, "min_threshold value (1) must be bigger or equal to 2"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_threshold' : 'x' }", O::Err(0x2000, "Invalid double value x for 'tombstone_threshold'"), O::Err(0x2300, "x is not a parsable int (base10) for tombstone_threshold")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_low' : 'x' }", O::Err(0x2000, "Invalid double value x for 'bucket_low'"), O::Err(0x2300, "x is not a parsable float for bucket_low")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_sstable_size' : 'x' }", O::Err(0x2000, "Invalid long value x for 'min_sstable_size'"), O::Err(0x2300, "x is not a parsable int (base10) for min_sstable_size")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_size' : 'x' }", O::Err(0x2000, "Invalid long value x for 'compaction_window_size'"), O::Err(0x2300, "x is not a parsable int (base10) for compaction_window_size")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_size' : 1 }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'expired_sstable_check_frequency_seconds' : -1 }", O::Ok, O::Err(0x2300, "expired_sstable_check_frequency_seconds must not be negative, but was -1")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'sstable_size_in_mb' : 'x' }", O::Err(0x2000, "Invalid integer value x for 'sstable_size_in_mb'"), O::Err(0x2300, "x is not a parsable int (base10) for sstable_size_in_mb")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'fanout_size' : 0 }", O::Err(0x2300, "Invalid compaction strategy options {\"fanout_size\": \"0\"} for chosen strategy type"), O::Err(0x2300, "fanout_size must be larger than 0, but was 0")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'log_all' : 'x' }", O::Err(0x2300, "Invalid compaction strategy options {\"log_all\": \"x\"} for chosen strategy type"), O::Err(0x2300, "'log_all' should either be 'true' or 'false', not x")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_compaction_interval' : 'x' }", O::Err(0x2000, "Invalid long value x for 'tombstone_compaction_interval'"), O::Err(0x2300, "x is not a parsable int (base10) for tombstone_compaction_interval")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 100 }", O::Err(0x2300, "chunk_length_in_kb/chunk_length_kb must be a power of 2."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 0 }", O::Err(0x2300, "Invalid negative or null for chunk_length_in_kb/chunk_length_kb"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': -1 }", O::Err(0x2300, "Invalid negative or null for chunk_length_in_kb/chunk_length_kb"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 'dog' }", O::Err(0x2000, "Invalid integer value dog for chunk_length_in_kb"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': null }", O::Err(0x2000, "Invalid integer value null for chunk_length_in_kb"), O::Err(0x2000, "Invalid property value: NULL for property: 'chunk_length_in_kb'")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 1048576 }", O::Err(0x2300, "chunk_length_in_kb/chunk_length_kb must be 128 or less."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 256 }", O::Err(0x2300, "chunk_length_in_kb/chunk_length_kb must be 128 or less."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 512 }", O::Err(0x2300, "chunk_length_in_kb/chunk_length_kb must be 128 or less."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 1.1 }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_kb': 4 }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'crc_check_chance': 2 }", O::Err(0x2300, "crc_check_chance must be between 0.0 and 1.0."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'foo': 2 }", O::Err(0x2300, "Unknown compression option 'foo'."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'enabled': false }", O::Err(0x2300, "Missing sub-option 'sstable_compression' for the 'compression' option."), O::Ok),
    ("ALTER TABLE ks.t WITH compression = { 'enabled': true }", O::Err(0x2300, "Missing sub-option 'sstable_compression' for the 'compression' option."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'enabled': 'x' }", O::Err(0x2300, "Missing sub-option 'sstable_compression' for the 'compression' option."), O::Ok),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': '' }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'class': 'LZ4Compressor' }", O::Err(0x2300, "Missing sub-option 'sstable_compression' for the 'compression' option."), O::Ok),
    ("ALTER TABLE ks.t WITH compression = { 'class': 'LZ4Compressor', 'chunk_length_in_kb': 100 }", O::Err(0x2300, "Missing sub-option 'sstable_compression' for the 'compression' option."), O::Err(0x2300, "chunk_length_in_kb must be a power of 2")),
    ("ALTER TABLE ks.t WITH compression = { }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = '+101.1PERCENTILE'", O::Err(0x2300, "Invalid value +101.1PERCENTILE for PERCENTILE option 'speculative_retry': must be between [0.0 and 100.0]"), O::Err(0x2300, "Invalid value +101.1PERCENTILE for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '-1PERCENTILE'", O::Err(0x2300, "Invalid value -1PERCENTILE for PERCENTILE option 'speculative_retry': must be between [0.0 and 100.0]"), O::Err(0x2300, "Invalid value -1PERCENTILE for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '-0.001PERCENTILE'", O::Err(0x2300, "Invalid value -0.001PERCENTILE for PERCENTILE option 'speculative_retry': must be between [0.0 and 100.0]"), O::Err(0x2300, "Invalid value -0.001PERCENTILE for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '100.001PERCENTILE'", O::Err(0x2300, "Invalid value 100.001PERCENTILE for PERCENTILE option 'speculative_retry': must be between [0.0 and 100.0]"), O::Err(0x2300, "Invalid value 100.001PERCENTILE for PERCENTILE option 'speculative_retry': must be between (0.0 and 100.0)")),
    ("ALTER TABLE ks.t WITH speculative_retry = '0PERCENTILE'", O::Ok, O::Err(0x2300, "Invalid value 0PERCENTILE for PERCENTILE option 'speculative_retry': must be between (0.0 and 100.0)")),
    ("ALTER TABLE ks.t WITH speculative_retry = 'DOGPERCENTILE'", O::Err(0x2300, "cannot convert DOGPERCENTILE to speculative_retry\n"), O::Err(0x2300, "Invalid value DOGPERCENTILE for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = 'dog'", O::Err(0x0000, "sstring out of range"), O::Err(0x2300, "Invalid value dog for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '50ms'", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = '+50PERCENTILE'", O::Ok, O::Err(0x2300, "Invalid value +50PERCENTILE for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '-5ms'", O::Ok, O::Err(0x2300, "Invalid value -5ms for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = 'xms'", O::Err(0x2300, "cannot convert XMS to speculative_retry\n"), O::Err(0x2300, "Invalid value xms for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '99p'", O::Err(0x0000, "sstring out of range"), O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = 'always'", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH min_index_interval = 0", O::Err(0x2300, "min_index_interval must be greater than 0"), O::Err(0x2300, "min_index_interval must be greater than or equal to 1 (got 0)")),
    ("ALTER TABLE ks.t WITH max_index_interval = 10", O::Err(0x2300, "max_index_interval must be greater than min_index_interval"), O::Err(0x2300, "max_index_interval must be greater than or equal to min_index_interval (128) (got 10)")),
    ("ALTER TABLE ks.t WITH bloom_filter_fp_chance = 0", O::Err(0x2300, "bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 0)"), O::Err(0x2300, "bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got 0.0)")),
    ("ALTER TABLE ks.t WITH bloom_filter_fp_chance = 1.5", O::Err(0x2300, "bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 1.5)"), O::Err(0x2300, "bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got 1.5)")),
    ("ALTER TABLE ks.t WITH caching = {'keys': 'SOME'}", O::Err(0x2300, "Invalid key value: SOME"), O::Err(0x2300, "Invalid value 'SOME' for caching sub-option 'keys': only 'ALL' and 'NONE' are allowed")),
    ("ALTER TABLE ks.t WITH caching = {'rows_per_partition': 'SOME'}", O::Err(0x2300, "Invalid key value: SOME"), O::Err(0x2300, "Invalid value 'SOME' for caching sub-option 'rows_per_partition': only 'ALL', 'NONE', and integer values are allowed")),
    ("ALTER TABLE ks.t WITH caching = {'rows_per_partition': '-1'}", O::Ok, O::Err(0x2300, "Invalid value '-1' for caching sub-option 'rows_per_partition': only 'ALL', 'NONE', and integer values are allowed")),
    ("ALTER TABLE ks.t WITH caching = {'foo': 'ALL'}", O::Err(0x2300, "Invalid caching option: foo"), O::Err(0x2300, "Invalid caching sub-options [foo]: only 'keys' and 'rows_per_partition' are allowed")),
    ("ALTER TABLE ks.t WITH caching = {'enabled': 'x'}", O::Ok, O::Err(0x2300, "Invalid caching sub-options [enabled]: only 'keys' and 'rows_per_partition' are allowed")),
    ("ALTER TABLE ks.t WITH gc_grace_seconds = -1", O::Ok, O::Err(0x2300, "gc_grace_seconds must be greater than or equal to 0 (got -1)")),
    ("ALTER TABLE ks.t WITH default_time_to_live = -1", O::Err(0x2300, "default_time_to_live cannot be smaller than 0, (default 0)"), O::Err(0x2300, "default_time_to_live must be greater than or equal to 0 (got -1)")),
    ("ALTER TABLE ks.t WITH memtable_flush_period_in_ms = -1", O::Err(0x2300, "memtable_flush_period_in_ms must be 0 or greater than 60000"), O::Err(0x2300, "memtable_flush_period_in_ms must be greater than or equal to 0 (got -1)")),
    ("ALTER TABLE ks.t WITH memtable_flush_period_in_ms = 5", O::Err(0x2300, "memtable_flush_period_in_ms must be 0 or greater than 60000"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_compaction_interval' : 0 }", O::Err(0x2300, "tombstone_compaction_interval value (0ms) must be positive"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_threshold' : 0 }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_threshold' : 1 }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_low' : 1.0 }", O::Err(0x2300, "bucket_low value (1) must be between 0.0 and 1.0"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_low' : 0.5, 'bucket_high' : 1.0 }", O::Err(0x2300, "bucket_high value (1) must be greater than 1.0"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_low' : 0.9, 'bucket_high' : 0.8 }", O::Err(0x2300, "bucket_high value (0.8) must be greater than 1.0"), O::Err(0x2300, "bucket_high value (0.8) is less than or equal to the bucket_low value (0.9)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_sstable_size' : 0 }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'cold_reads_to_omit' : 1.0 }", O::Ok, O::Err(0x2300, "Properties specified [cold_reads_to_omit] are not understood by SizeTieredCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'cold_reads_to_omit' : 0 }", O::Ok, O::Err(0x2300, "Properties specified [cold_reads_to_omit] are not understood by SizeTieredCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'enabled' : 'TRUE' }", O::Err(0x2300, "enabled value (TRUE) must be \"true\" or \"false\""), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'unchecked_tombstone_compaction' : 'True' }", O::Err(0x2300, "unchecked_tombstone_compaction value (True) must be \"true\" or \"false\""), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_threshold' : 'x' }", O::Err(0x2000, "Invalid long value x for 'min_threshold'"), O::Err(0x2300, "Invalid value x for 'min_threshold' compaction sub-option - must be an integer")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'max_threshold' : 'x' }", O::Err(0x2000, "Invalid long value x for 'max_threshold'"), O::Err(0x2300, "Invalid value x for 'max_threshold' compaction sub-option - must be an integer")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_threshold' : 5, 'max_threshold' : 3 }", O::Ok, O::Err(0x2300, "Min compaction threshold (got 5) cannot be greater than max compaction threshold (got 3)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_threshold' : 0 }", O::Err(0x2300, "min_threshold value (0) must be bigger or equal to 2"), O::Err(0x2300, "Disabling compaction by setting compaction thresholds to 0 has been removed, set the compaction option 'enabled' to false instead.")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_threshold' : 40 }", O::Ok, O::Err(0x2300, "Min compaction threshold (got 40) cannot be greater than max compaction threshold (got 32)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'only_purge_repaired_tombstones' : 'x' }", O::Err(0x2300, "Invalid compaction strategy options {\"only_purge_repaired_tombstones\": \"x\"} for chosen strategy type"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'provide_overlapping_tombstones' : 'x' }", O::Err(0x2300, "Invalid compaction strategy options {\"provide_overlapping_tombstones\": \"x\"} for chosen strategy type"), O::Err(0x2300, "Invalid value X for 'provide_overlapping_tombstones' compaction sub-option - must be one of the following [NONE, ROW, CELL].")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_size' : 0 }", O::Err(0x2300, "compaction_window_size value (0) must be greater than 1"), O::Err(0x2300, "0 must be greater than 1 for compaction_window_size")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'timestamp_resolution' : 'MICROSECONDS' }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'timestamp_resolution' : 'microseconds' }", O::Err(0x2300, "Invalid timestamp resolution microseconds for timestamp_resolution"), O::Err(0x2300, "microseconds is not valid for timestamp_resolution")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'timestamp_resolution' : 'MILLISECONDS' }", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'timestamp_resolution' : 'NANOSECONDS' }", O::Err(0x2300, "Invalid timestamp resolution NANOSECONDS for timestamp_resolution"), O::Ok),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_unit' : 'days' }", O::Err(0x2300, "Invalid window unit days for compaction_window_unit"), O::Err(0x2300, "days is not valid for compaction_window_unit")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'enable_optimized_twcs_queries' : 'TRUE' }", O::Err(0x2300, "enable_optimized_twcs_queries value (TRUE) must be \"true\" or \"false\""), O::Err(0x2300, "Properties specified [enable_optimized_twcs_queries] are not understood by TimeWindowCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'expired_sstable_check_frequency_seconds' : 'x' }", O::Err(0x2000, "Invalid long value x for expired_sstable_check_frequency_seconds"), O::Err(0x2300, "x is not a parsable int (base10) for expired_sstable_check_frequency_seconds")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'unsafe_aggressive_sstable_expiration' : 'x' }", O::Ok, O::Err(0x2300, "unsafe_aggressive_sstable_expiration is not 'true' or 'false' (x)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'min_sstable_size' : -1 }", O::Err(0x2300, "min_sstable_size value (-1) must be non negative"), O::Err(0x2300, "min_sstable_size must be non negative: -1")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'sstable_size_in_mb' : 0 }", O::Err(0x2300, "sstable_size_in_mb value (0) must be positive"), O::Err(0x2300, "sstable_size_in_mb must be larger than 0, but was 0")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'min_threshold' : 1 }", O::Err(0x2300, "min_threshold value (1) must be bigger or equal to 2"), O::Err(0x2300, "Min compaction threshold cannot be less than 2 (got 1)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'LeveledCompactionStrategy', 'bucket_low' : 5 }", O::Err(0x2300, "bucket_low value (5) must be between 0.0 and 1.0"), O::Err(0x2300, "bucket_high value (1.5) is less than or equal to the bucket_low value (5.0)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'space_amplification_goal' : 2.0 }", O::Ok, O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'space_amplification_goal' : 'x' }", O::Err(0x2000, "Invalid double value x for 'space_amplification_goal'"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'sstable_size_in_mb' : -1 }", O::Ok, O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'sstable_size_in_mb' : 0 }", O::Ok, O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'cold_reads_to_omit' : 0.5 }", O::Err(0x2300, "Invalid compaction strategy options {\"cold_reads_to_omit\": \"0.5\"} for chosen strategy type"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'IncrementalCompactionStrategy', 'tombstone_threshold' : 3 }", O::Err(0x2300, "tombstone_threshold value (3) must be between 0.0 and 1.0"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'NullCompactionStrategy', 'tombstone_threshold' : 3 }", O::Err(0x2300, "tombstone_threshold value (3) must be between 0.0 and 1.0"), O::Err(0x2300, "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.NullCompactionStrategy'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_threshold' : 0.5, 'bucket_low' : 2, 'min_sstable_size' : -1 }", O::Err(0x2300, "min_sstable_size value (-1) must be non negative"), O::Err(0x2300, "min_sstable_size must be non negative: -1")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'TimeWindowCompactionStrategy', 'compaction_window_size' : -8, 'compaction_window_unit' : 'SECONDS' }", O::Err(0x2300, "Invalid window unit SECONDS for compaction_window_unit"), O::Err(0x2300, "SECONDS is not valid for compaction_window_unit")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'bucket_high' : 0.7, 'bucket_low' : 1.3 }", O::Err(0x2300, "bucket_low value (1.3) must be between 0.0 and 1.0"), O::Err(0x2300, "bucket_high value (0.7) is less than or equal to the bucket_low value (1.3)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'tombstone_threshold' : 5, 'unchecked_tombstone_compaction': 'x' }", O::Err(0x2300, "tombstone_threshold value (5) must be between 0.0 and 1.0"), O::Err(0x2300, "'unchecked_tombstone_compaction' should be either 'true' or 'false', not 'x'")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'min_threshold' : 1, 'tombstone_threshold' : 5 }", O::Err(0x2300, "tombstone_threshold value (5) must be between 0.0 and 1.0"), O::Err(0x2300, "Min compaction threshold cannot be less than 2 (got 1)")),
    ("ALTER TABLE ks.t WITH compaction = { 'class' : 'SizeTieredCompactionStrategy', 'foo' : 1, 'tombstone_threshold' : 5 }", O::Err(0x2300, "tombstone_threshold value (5) must be between 0.0 and 1.0"), O::Err(0x2300, "Properties specified [foo] are not understood by SizeTieredCompactionStrategy")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_kb': 'dog' }", O::Err(0x2000, "Invalid integer value dog for chunk_length_in_kb"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 4, 'chunk_length_kb': 8 }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 128 }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 1 }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'crc_check_chance': 'x' }", O::Err(0x2000, "Invalid double value x for crc_check_chance"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'ZstdCompressor', 'compression_level': 3 }", O::Ok, O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'ZstdCompressor', 'compression_level': 99 }", O::Err(0x2300, "-131072 must be between 22 and compression_level, got 99"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'compression_level': 3 }", O::Err(0x2300, "Unknown compression option 'compression_level'."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'DeflateCompressor', 'chunk_length_in_kb': 3 }", O::Err(0x2300, "chunk_length_in_kb/chunk_length_kb must be a power of 2."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'enabled': 'false' }", O::Err(0x2300, "Unknown compression option 'enabled'."), O::Err(0x2300, "If the 'enabled' option is set to false no other options must be specified")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 3, 'foo': 'x' }", O::Err(0x2300, "Unknown compression option 'foo'."), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH compression = { 'sstable_compression': 'Foo', 'chunk_length_in_kb': 3 }", O::Err(0x0000, "Unknown sstable_compression: Foo"), O::Err(0x2300, "Missing sub-option 'class' for the 'compression' option.")),
    ("ALTER TABLE ks.t WITH speculative_retry = '50.5PERCENTILE'", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = '100PERCENTILE'", O::Ok, O::Err(0x2300, "Invalid value 100PERCENTILE for PERCENTILE option 'speculative_retry': must be between (0.0 and 100.0)")),
    ("ALTER TABLE ks.t WITH speculative_retry = '50percentile'", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = '50.5ms'", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = 'none'", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = 'NEVER'", O::Err(0x0000, "sstring out of range"), O::Ok),
    ("ALTER TABLE ks.t WITH speculative_retry = 'ALWAYSX'", O::Err(0x0000, "sstring out of range"), O::Err(0x2300, "Invalid value ALWAYSX for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = ''", O::Err(0x0000, "sstring out of range"), O::Err(0x2300, "Invalid value  for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH speculative_retry = '5'", O::Err(0x0000, "sstring out of range"), O::Err(0x2300, "Invalid value 5 for option 'speculative_retry'")),
    ("ALTER TABLE ks.t WITH min_index_interval = 200", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH max_index_interval = 200", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH max_index_interval = 199", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH bloom_filter_fp_chance = 1", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH bloom_filter_fp_chance = 0.0000671", O::Err(0x2300, "bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 6.71e-05)"), O::Err(0x2300, "bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got 6.71E-5)")),
    ("ALTER TABLE ks.t WITH caching = {'keys': 'none', 'rows_per_partition': '10'}", O::Err(0x2300, "Invalid key value: none"), O::Ok),
    ("ALTER TABLE ks.t WITH caching = {'keys': 'ALL', 'rows_per_partition': '0'}", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH caching = {}", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH caching = {'enabled': 'false'}", O::Ok, O::Err(0x2300, "Invalid caching sub-options [enabled]: only 'keys' and 'rows_per_partition' are allowed")),
    ("ALTER TABLE ks.t WITH memtable_flush_period_in_ms = 60000", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH memtable_flush_period_in_ms = 60001", O::Ok, O::Ok),
    ("ALTER TABLE ks.t WITH crc_check_chance = 2", O::Ok, O::Err(0x2300, "crc_check_chance must be larger than or equal to 0 and smaller than or equal to 1.0 (got 2.0)")),
    ("ALTER TABLE ks.t WITH crc_check_chance = -1", O::Ok, O::Err(0x2300, "crc_check_chance must be larger than or equal to 0 and smaller than or equal to 1.0 (got -1.0)")),
    ("ALTER TABLE ks.t WITH default_time_to_live = 630720001", O::Ok, O::Err(0x2300, "default_time_to_live must be less than or equal to 630720000 (got 630720001)")),
    ("ALTER TABLE ks.t WITH gc_grace_seconds = 'x'", O::Err(0x2000, "Invalid integer value x for 'gc_grace_seconds'"), O::Err(0x2000, "Invalid integer value x for 'gc_grace_seconds'")),
];

#[tokio::test]
async fn alter_table_options() {
    let mut bad = Vec::new();
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!("{KS};\nCREATE TABLE ks.t (a int PRIMARY KEY, b int)")).await;
        for (cql, scylla, cassandra) in CASES {
            let want = if d.is_scylla() { scylla } else { cassandra };
            let got = h.run(cql).await;
            let ok = match (want, &got) {
                (O::Ok, Ok(_)) => true,
                (O::Err(code, msg), Err(e)) => e.code.code() == *code && e.message == *msg,
                _ => false,
            };
            if !ok {
                let got = match got {
                    Ok(_) => "Ok".to_string(),
                    Err(e) => format!("0x{:04X} {:?}", e.code.code(), e.message),
                };
                bad.push(format!("{d}: {cql}\n    want {want:?}\n    got  {got}"));
            }
        }
    }
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

/// How `compression` maps are stored.
#[tokio::test]
async fn compression_maps() {
    let stored = "SELECT compression FROM system_schema.tables WHERE keyspace_name = 'ks' AND table_name = 't'";
    for (d, cases) in [
        (
            Dialect::Scylla,
            &[
                ("{'sstable_compression': 'LZ4Compressor', 'chunk_length_kb': 4}", "{'chunk_length_in_kb': '4', 'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}"),
                ("{'sstable_compression': ''}", "{}"),
                ("{}", "{}"),
                ("{'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': 1.1}", "{'chunk_length_in_kb': '1', 'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}"),
            ][..],
        ),
        (
            Dialect::Cassandra,
            &[
                ("{'enabled': false}", "{'enabled': 'false'}"),
                ("{'class': 'LZ4Compressor'}", "{'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}"),
                ("{}", "{'enabled': 'false'}"),
            ][..],
        ),
    ] {
        let mut h = H::new(d).await;
        h.exec(&format!("{KS};\nCREATE TABLE ks.t (a int PRIMARY KEY, b int)")).await;
        for (m, want) in cases {
            h.ok(&format!("ALTER TABLE ks.t WITH compression = {m}")).await;
            assert_eq!(h.rows(stored).await, table(&[&[want]]), "{d}: {m}");
        }
    }
}
