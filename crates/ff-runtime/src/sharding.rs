//! Multi-Spark sharding map for ultra-large MoE models.
//!
//! Some models are far too large to serve from a single DGX Spark (128 GB
//! unified memory) even after aggressive quantization. To serve them locally
//! we partition the model's transformer decoder layers across a **ring of
//! Sparks** and stream activations around the ring (pipeline / ring
//! parallelism), rather than the tensor parallelism used for a single linked
//! Spark pair in [`crate::vllm`].
//!
//! This module defines that partitioning as pure, testable data:
//! - which contiguous layer range each Spark owns ([`SparkShard`]),
//! - the directed cross-Spark communication paths that close the ring
//!   ([`RingLink`]),
//! - the weight footprint accounting so a caller can check each shard fits.
//!
//! It intentionally holds no process or I/O state — it is the *plan* a launcher
//! (e.g. a llama.cpp RPC or vLLM `--pipeline-parallel-size` deployment) consumes.
//!
//! Layer-count-proportional byte sizing (see [`ShardingMap::partition`]) assumes
//! roughly uniform per-layer weight size. For MoE architectures like Kimi K2
//! Thinking — whose leading layer(s) are dense while the rest are much heavier
//! MoE layers with hundreds of routed experts — this under/over-estimates the
//! true per-shard footprint slightly; the accounting here is meant as a
//! capacity-planning approximation, not an exact placement of individual expert
//! tensors.

/// Transformer decoder layers in Qwen3-Coder-480B-A35B (published config
/// `num_hidden_layers`).
pub const QWEN3_CODER_480B_LAYERS: u32 = 62;

/// Approximate on-disk / in-memory footprint of the Q4-quantized weights
/// (~276 GB). Used to size per-Spark shards.
pub const QWEN3_CODER_480B_Q4_BYTES: u64 = 276 * 1024 * 1024 * 1024;

/// Transformer decoder layers in Kimi K2 Thinking (DeepSeek-V3-style
/// architecture: 1 dense layer followed by MoE layers, 61 total).
pub const KIMI_K2_THINKING_LAYERS: u32 = 61;

/// Approximate on-disk / in-memory footprint of the Unsloth-style dynamic
/// 1.8-bit ("UD-TQ1") quantized weights (~245 GB). Used to size per-Spark
/// shards.
pub const KIMI_K2_THINKING_UD_TQ1_BYTES: u64 = 245 * 1024 * 1024 * 1024;

/// Number of DGX Sparks the 480B / Kimi K2 Thinking models are partitioned
/// across. Two Sparks (256 GB) leaves only ~11 GB of headroom for a 245 GB
/// model once split evenly — too tight for KV cache and framework overhead —
/// so the canonical ring uses 3 Sparks (384 GB, ~46 GB headroom per Spark).
pub const SPARK_RING_SIZE: usize = 3;

/// Weight quantization scheme, used to reason about a shard's footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quant {
    /// 4-bit weights.
    Q4,
    /// 8-bit weights (~2x Q4).
    Q8,
    /// 16-bit weights (~4x Q4).
    F16,
    /// Ultra-low-precision dynamic ~1.8-bit weights (e.g. Unsloth's
    /// `UD-TQ1_0` dynamic quant used for Kimi K2 Thinking, ~245 GB total).
    UdTq1_8,
}

impl Quant {
    /// Short catalog-style tag (`"q4"`, `"q8"`, `"f16"`, `"ud-tq1"`).
    pub fn tag(self) -> &'static str {
        match self {
            Quant::Q4 => "q4",
            Quant::Q8 => "q8",
            Quant::F16 => "f16",
            Quant::UdTq1_8 => "ud-tq1",
        }
    }

    /// Approximate bits stored per weight for this scheme.
    pub fn bits_per_weight(self) -> f64 {
        match self {
            Quant::Q4 => 4.0,
            Quant::Q8 => 8.0,
            Quant::F16 => 16.0,
            Quant::UdTq1_8 => 1.8,
        }
    }
}

/// One Spark's slice of the model: a contiguous, inclusive range of decoder
/// layers plus its approximate weight footprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparkShard {
    /// Position of this Spark in the ring, `0..ring_size`.
    pub spark_index: usize,
    /// First decoder layer owned by this Spark (inclusive, 0-based).
    pub first_layer: u32,
    /// Last decoder layer owned by this Spark (inclusive).
    pub last_layer: u32,
    /// Number of layers owned by this Spark (`last - first + 1`).
    pub layer_count: u32,
    /// Approximate bytes of quantized weights resident on this Spark.
    pub approx_bytes: u64,
}

impl SparkShard {
    /// Whether `layer` falls inside this shard's owned range.
    pub fn owns_layer(&self, layer: u32) -> bool {
        layer >= self.first_layer && layer <= self.last_layer
    }
}

/// A directed activation hand-off between two Sparks. The ring is closed:
/// forward links carry hidden states from one pipeline stage to the next, and a
/// single wrap-around link returns the final stage's output to the first Spark
/// (which owns the embeddings / LM head) for the next autoregressive step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingLink {
    /// Spark emitting the activations.
    pub from_spark: usize,
    /// Spark receiving the activations.
    pub to_spark: usize,
    /// Layer index after which the hand-off occurs (the sender's last layer).
    pub boundary_layer: u32,
    /// True for the single `last -> 0` link that closes the ring.
    pub wrap_around: bool,
}

/// Full sharding plan for one model at one quantization across a Spark ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardingMap {
    /// Catalog model id (e.g. `"qwen3-coder-480b"`).
    pub model_id: String,
    /// Quantization the byte accounting is sized for.
    pub quant: Quant,
    /// Total decoder layers being partitioned.
    pub total_layers: u32,
    /// Total quantized weight bytes across all shards.
    pub total_bytes: u64,
    /// Per-Spark layer ownership, ordered by `spark_index`.
    pub shards: Vec<SparkShard>,
    /// Directed communication paths forming the closed ring.
    pub ring_links: Vec<RingLink>,
}

impl ShardingMap {
    /// The canonical plan: Qwen3-Coder-480B, Q4, across 3 Sparks.
    pub fn qwen3_coder_480b_q4() -> Self {
        Self::partition(
            "qwen3-coder-480b",
            Quant::Q4,
            QWEN3_CODER_480B_LAYERS,
            QWEN3_CODER_480B_Q4_BYTES,
            SPARK_RING_SIZE,
        )
    }

    /// The canonical plan: Kimi K2 Thinking, dynamic 1.8-bit (`UD-TQ1_0`),
    /// across 3 Sparks.
    pub fn kimi_k2_thinking_ud_tq1() -> Self {
        Self::partition(
            "kimi-k2-thinking",
            Quant::UdTq1_8,
            KIMI_K2_THINKING_LAYERS,
            KIMI_K2_THINKING_UD_TQ1_BYTES,
            SPARK_RING_SIZE,
        )
    }

    /// Build a ring-partitioned sharding map, distributing `total_layers` as
    /// evenly as possible across `ring_size` Sparks (earlier Sparks absorb the
    /// remainder), sizing each shard's bytes proportionally to its layer count,
    /// and wiring the forward + wrap-around ring links.
    ///
    /// # Panics
    /// Panics if `ring_size == 0` or `total_layers < ring_size` (a ring needs at
    /// least one layer per Spark).
    pub fn partition(
        model_id: impl Into<String>,
        quant: Quant,
        total_layers: u32,
        total_bytes: u64,
        ring_size: usize,
    ) -> Self {
        assert!(ring_size > 0, "ring_size must be non-zero");
        assert!(
            total_layers as usize >= ring_size,
            "need at least one layer per Spark: {total_layers} layers < {ring_size} Sparks"
        );

        let ring_u32 = ring_size as u32;
        let base = total_layers / ring_u32;
        let remainder = total_layers % ring_u32;

        let mut shards = Vec::with_capacity(ring_size);
        let mut next_first = 0u32;
        let mut bytes_assigned = 0u64;
        for spark_index in 0..ring_size {
            // Earlier Sparks take one extra layer until the remainder is spent.
            let extra = u32::from((spark_index as u32) < remainder);
            let layer_count = base + extra;
            let first_layer = next_first;
            let last_layer = first_layer + layer_count - 1;
            next_first = last_layer + 1;

            // Size bytes proportionally; the final shard absorbs any rounding
            // drift so the shard bytes sum exactly to `total_bytes`.
            let approx_bytes = if spark_index + 1 == ring_size {
                total_bytes - bytes_assigned
            } else {
                let b = (total_bytes as u128 * layer_count as u128 / total_layers as u128) as u64;
                bytes_assigned += b;
                b
            };

            shards.push(SparkShard {
                spark_index,
                first_layer,
                last_layer,
                layer_count,
                approx_bytes,
            });
        }

        let ring_links = Self::build_ring_links(&shards, total_layers);

        Self {
            model_id: model_id.into(),
            quant,
            total_layers,
            total_bytes,
            shards,
            ring_links,
        }
    }

    fn build_ring_links(shards: &[SparkShard], total_layers: u32) -> Vec<RingLink> {
        let ring_size = shards.len();
        // A single Spark holds the whole model — no cross-Spark hand-off.
        if ring_size < 2 {
            return Vec::new();
        }
        let mut links = Vec::with_capacity(ring_size);
        for (i, shard) in shards.iter().enumerate() {
            let to = (i + 1) % ring_size;
            let wrap_around = to == 0;
            // Forward links hand off at the sender's last layer; the wrap link
            // returns the model's final output to Spark 0.
            let boundary_layer = if wrap_around {
                total_layers - 1
            } else {
                shard.last_layer
            };
            links.push(RingLink {
                from_spark: shard.spark_index,
                to_spark: to,
                boundary_layer,
                wrap_around,
            });
        }
        links
    }

    /// Which Spark owns `layer`, if any.
    pub fn spark_for_layer(&self, layer: u32) -> Option<&SparkShard> {
        self.shards.iter().find(|s| s.owns_layer(layer))
    }

    /// The single wrap-around link that closes the ring (`last -> 0`), if the
    /// ring spans more than one Spark.
    pub fn wrap_link(&self) -> Option<&RingLink> {
        self.ring_links.iter().find(|l| l.wrap_around)
    }

    /// True iff the shards cover exactly `[0, total_layers)` with no gap or
    /// overlap and the shard bytes sum to `total_bytes` — an integrity check
    /// for any hand-built or future map.
    pub fn is_contiguous(&self) -> bool {
        let mut expected_first = 0u32;
        let mut bytes = 0u64;
        for (i, shard) in self.shards.iter().enumerate() {
            if shard.spark_index != i
                || shard.first_layer != expected_first
                || shard.last_layer < shard.first_layer
                || shard.layer_count != shard.last_layer - shard.first_layer + 1
            {
                return false;
            }
            expected_first = shard.last_layer + 1;
            bytes += shard.approx_bytes;
        }
        expected_first == self.total_layers && bytes == self.total_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quant_tags_and_bits() {
        assert_eq!(Quant::Q4.tag(), "q4");
        assert_eq!(Quant::Q4.bits_per_weight(), 4.0);
        assert_eq!(Quant::UdTq1_8.tag(), "ud-tq1");
        assert_eq!(Quant::UdTq1_8.bits_per_weight(), 1.8);
        assert!(Quant::F16.bits_per_weight() > Quant::Q8.bits_per_weight());
        assert!(Quant::Q4.bits_per_weight() > Quant::UdTq1_8.bits_per_weight());
    }

    #[test]
    fn qwen_480b_partitions_62_layers_across_3_sparks() {
        let map = ShardingMap::qwen3_coder_480b_q4();
        assert_eq!(map.model_id, "qwen3-coder-480b");
        assert_eq!(map.quant, Quant::Q4);
        assert_eq!(map.total_layers, 62);
        assert_eq!(map.shards.len(), 3);

        // 62 = 21 + 21 + 20; remainder falls on the earlier Sparks.
        let counts: Vec<u32> = map.shards.iter().map(|s| s.layer_count).collect();
        assert_eq!(counts, vec![21, 21, 20]);
        assert_eq!(counts.iter().sum::<u32>(), 62);
    }

    #[test]
    fn shards_are_contiguous_and_bytes_sum_exactly() {
        let map = ShardingMap::qwen3_coder_480b_q4();
        assert!(map.is_contiguous());

        let first = &map.shards[0];
        assert_eq!(first.first_layer, 0);
        let last = map.shards.last().unwrap();
        assert_eq!(last.last_layer, 61);

        let byte_sum: u64 = map.shards.iter().map(|s| s.approx_bytes).sum();
        assert_eq!(byte_sum, QWEN3_CODER_480B_Q4_BYTES);
    }

    #[test]
    fn ring_links_close_the_ring() {
        let map = ShardingMap::qwen3_coder_480b_q4();
        assert_eq!(map.ring_links.len(), 3);

        // Forward links: 0->1, 1->2 at each sender's last layer.
        assert_eq!(map.ring_links[0].from_spark, 0);
        assert_eq!(map.ring_links[0].to_spark, 1);
        assert_eq!(map.ring_links[0].boundary_layer, map.shards[0].last_layer);
        assert!(!map.ring_links[0].wrap_around);

        assert_eq!(map.ring_links[1].from_spark, 1);
        assert_eq!(map.ring_links[1].to_spark, 2);

        // Wrap link: 2->0 at the model's final layer.
        let wrap = map.wrap_link().expect("wrap link");
        assert_eq!(wrap.from_spark, 2);
        assert_eq!(wrap.to_spark, 0);
        assert!(wrap.wrap_around);
        assert_eq!(wrap.boundary_layer, map.total_layers - 1);
    }

    #[test]
    fn spark_for_layer_maps_boundaries() {
        let map = ShardingMap::qwen3_coder_480b_q4();
        assert_eq!(map.spark_for_layer(0).unwrap().spark_index, 0);
        assert_eq!(map.spark_for_layer(20).unwrap().spark_index, 0);
        assert_eq!(map.spark_for_layer(21).unwrap().spark_index, 1);
        assert_eq!(map.spark_for_layer(41).unwrap().spark_index, 1);
        assert_eq!(map.spark_for_layer(42).unwrap().spark_index, 2);
        assert_eq!(map.spark_for_layer(61).unwrap().spark_index, 2);
        assert!(map.spark_for_layer(62).is_none());
    }

    #[test]
    fn uneven_partition_distributes_remainder_to_earlier_sparks() {
        let map = ShardingMap::partition("m", Quant::Q4, 10, 1000, 3);
        let counts: Vec<u32> = map.shards.iter().map(|s| s.layer_count).collect();
        assert_eq!(counts, vec![4, 3, 3]);
        assert!(map.is_contiguous());
        assert_eq!(map.shards.iter().map(|s| s.approx_bytes).sum::<u64>(), 1000);
    }

    #[test]
    fn single_spark_has_no_ring_links() {
        let map = ShardingMap::partition("m", Quant::Q8, 8, 800, 1);
        assert_eq!(map.shards.len(), 1);
        assert_eq!(map.shards[0].layer_count, 8);
        assert!(map.ring_links.is_empty());
        assert!(map.wrap_link().is_none());
        assert!(map.is_contiguous());
    }

    #[test]
    #[should_panic(expected = "at least one layer per Spark")]
    fn partition_rejects_more_sparks_than_layers() {
        ShardingMap::partition("m", Quant::Q4, 2, 100, 3);
    }

    #[test]
    fn kimi_k2_thinking_partitions_61_layers_across_3_sparks() {
        let map = ShardingMap::kimi_k2_thinking_ud_tq1();
        assert_eq!(map.model_id, "kimi-k2-thinking");
        assert_eq!(map.quant, Quant::UdTq1_8);
        assert_eq!(map.total_layers, 61);
        assert_eq!(map.total_bytes, KIMI_K2_THINKING_UD_TQ1_BYTES);
        assert_eq!(map.shards.len(), 3);

        // 61 = 21 + 20 + 20; remainder falls on the earlier Spark.
        let counts: Vec<u32> = map.shards.iter().map(|s| s.layer_count).collect();
        assert_eq!(counts, vec![21, 20, 20]);
        assert_eq!(counts.iter().sum::<u32>(), 61);
        assert!(map.is_contiguous());
    }

    #[test]
    fn kimi_k2_thinking_shard_bytes_fit_a_single_spark() {
        let map = ShardingMap::kimi_k2_thinking_ud_tq1();
        // DGX Spark unified memory (128 GB) per node; each shard must leave
        // room for KV cache / framework overhead on top of resident weights.
        const SPARK_MEMORY_BYTES: u64 = 128 * 1024 * 1024 * 1024;
        for shard in &map.shards {
            assert!(shard.approx_bytes < SPARK_MEMORY_BYTES);
        }

        // Splitting the same ~245 GB across only 2 Sparks would leave under
        // 12 GB of headroom per Spark (256 GB - 245 GB) — too tight for KV
        // cache, so the canonical ring uses 3.
        let two_spark = ShardingMap::partition(
            "kimi-k2-thinking",
            Quant::UdTq1_8,
            KIMI_K2_THINKING_LAYERS,
            KIMI_K2_THINKING_UD_TQ1_BYTES,
            2,
        );
        for shard in &two_spark.shards {
            let headroom = SPARK_MEMORY_BYTES.saturating_sub(shard.approx_bytes);
            assert!(headroom < 16 * 1024 * 1024 * 1024);
        }
    }

    #[test]
    fn kimi_k2_thinking_ring_links_close_the_ring() {
        let map = ShardingMap::kimi_k2_thinking_ud_tq1();
        assert_eq!(map.ring_links.len(), 3);
        let wrap = map.wrap_link().expect("wrap link");
        assert_eq!(wrap.from_spark, 2);
        assert_eq!(wrap.to_spark, 0);
        assert_eq!(wrap.boundary_layer, map.total_layers - 1);
    }
}
