//! Focused unit tests for dense-search determinism and numerical safety.
//!
//! Three contracts are pinned here:
//! 1. **Shared total order** (`dense_total_cmp` / `score_desc`): descending
//!    score, then ascending stable page/passage id bytes, so equal scores are
//!    ordered deterministically (no `unwrap_or(Equal)` collapse).
//! 2. **Top-k oracle**: the `select_nth_unstable_by(limit, cmp)` +
//!    `truncate` + full `sort_by` pipeline used by `top_embedding_hits_*`
//!    yields exactly the same head as a full sort for k in {0,1,n-1,n,n+1}.
//! 3. **Numerical safety**: f64 accumulation in `l2_normalize`/`dot_f32`/
//!    `dot_embedding_bytes` must not overflow to `inf`, and non-finite
//!    vectors/bytes must be rejected as `MalformedRecord`, never propagated
//!    as `NaN`/`inf` scores.
use super::*;

fn page_id(seed: u128) -> PageId {
    PageId::from_slice(&seed.to_be_bytes()).unwrap()
}

fn path(s: &str) -> PagePath {
    PagePath::new(s.to_string()).unwrap()
}

fn hit(id: u128, score: f32) -> (PageId, PagePath, f32) {
    (page_id(id), path(&format!("{id}.md")), score)
}

/// Replicate the exact top-k pipeline used by `top_embedding_hits_in_table`
/// so we can oracle it against a full sort.
fn top_k_pipeline(
    mut items: Vec<(PageId, PagePath, f32)>,
    limit: usize,
) -> Vec<(PageId, PagePath, f32)> {
    if items.len() > limit {
        items.select_nth_unstable_by(limit, score_desc);
        items.truncate(limit);
    }
    items.sort_by(score_desc);
    items
}

#[test]
fn score_desc_orders_by_score_then_ascending_page_id() {
    // Same score, different ids -> ascending id wins, deterministically.
    let a = hit(0x10, 0.5);
    let b = hit(0x02, 0.5);
    assert_eq!(
        score_desc(&a, &b),
        Ordering::Greater,
        "higher id, equal score sorts after"
    );
    assert_eq!(
        score_desc(&b, &a),
        Ordering::Less,
        "lower id, equal score sorts first"
    );
    // Different scores -> higher score first regardless of id.
    let hi = hit(0xFF, 0.9);
    let lo = hit(0x01, 0.1);
    assert_eq!(score_desc(&hi, &lo), Ordering::Less, "higher score first");
    assert_eq!(score_desc(&lo, &hi), Ordering::Greater);
    // Identical id+score -> Equal (total comparator still yields a stable result).
    assert_eq!(score_desc(&a, &hit(0x10, 0.5)), Ordering::Equal);
}

#[test]
fn top_k_oracle_matches_full_sort_at_boundaries() {
    // Deliberately messy: repeated scores, out-of-id-order.
    let items = vec![
        hit(0x30, 0.9),
        hit(0x10, 0.5),
        hit(0x20, 0.9),
        hit(0x40, 0.5),
        hit(0x50, 0.3),
        hit(0x60, 0.5),
    ];
    let n = items.len();
    let mut full = items.clone();
    full.sort_by(score_desc);
    for limit in [0usize, 1, n - 1, n, n + 1] {
        let got = top_k_pipeline(items.clone(), limit);
        assert_eq!(
            got,
            full[..limit.min(n)],
            "top-{limit} must equal full-sort head"
        );
    }
}

#[test]
fn equal_score_shuffle_yields_identical_ranking_every_time() {
    // Same score set, several permutation orders -> identical final order.
    let scores = [0.5f32, 0.5, 0.5, 0.5, 0.5];
    let ids = [0xAAu128, 0x01, 0x7F, 0x00, 0x99];
    let base: Vec<(PageId, PagePath, f32)> =
        ids.iter().zip(scores).map(|(id, s)| hit(*id, s)).collect();
    let mut sorted = base.clone();
    sorted.sort_by(score_desc);
    // ascending id order among the ties (big-endian seed == id byte ordering)
    let mut by_id = ids.to_vec();
    by_id.sort_unstable();
    let got_seed_order: Vec<u128> = sorted
        .iter()
        .map(|(p, _, _)| {
            let b: [u8; 16] = p.as_bytes().to_owned();
            u128::from_be_bytes(b)
        })
        .collect();
    assert_eq!(got_seed_order, by_id, "ties resolve by ascending id");
    // reversed and forward permutations all sort identically
    let mut rev = base.clone();
    rev.reverse();
    rev.sort_by(score_desc);
    assert_eq!(rev, sorted);
    let mut rand = vec![
        base[3].clone(),
        base[0].clone(),
        base[4].clone(),
        base[1].clone(),
        base[2].clone(),
    ];
    rand.sort_by(score_desc);
    assert_eq!(rand, sorted);
}

#[test]
fn l2_normalize_accumulates_in_f64_without_overflow() {
    // Huge magnitude, still unit-normalised without reaching inf.
    let v = vec![f32::MAX, f32::MAX, f32::MAX];
    let n = l2_normalize(&v).unwrap();
    let norm: f64 = n.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    assert!((norm - 1.0).abs() < 1e-6, "norm must be ~1, got {norm}");
    assert!(
        n.iter().all(|x| x.is_finite()),
        "no inf/nan in normalized output"
    );
    // Zero vector -> unchanged (unit vector of length 0, existing behaviour).
    assert_eq!(l2_normalize(&[0.0, 0.0]).unwrap(), vec![0.0, 0.0]);
    // Ordinary vector.
    let three_four = l2_normalize(&[3.0, 4.0]).unwrap();
    assert!((three_four[0] - 0.6).abs() < 1e-6);
    assert!((three_four[1] - 0.8).abs() < 1e-6);
}

#[test]
fn l2_normalize_rejects_non_finite() {
    assert!(matches!(
        l2_normalize(&[1.0, f32::NAN]),
        Err(StoreError::Memory(
            sessionmunch_core::MemoryError::MalformedRecord(_)
        ))
    ));
    assert!(l2_normalize(&[f32::INFINITY, 1.0]).is_err());
    assert!(l2_normalize(&[1.0, f32::NEG_INFINITY]).is_err());
}

#[test]
fn dot_f32_accumulates_in_f64_and_rejects_non_finite() {
    // Ordinary finite args accumulate exactly.
    assert_eq!(dot_f32(&[1.0, 2.0], &[3.0, 4.0]).unwrap(), 11.0);
    // mismatch just zips to min len, still finite
    assert_eq!(dot_f32(&[1.0, 2.0, 3.0], &[1.0, 2.0]).unwrap(), 5.0);
    // Large finite args that stay within f32 range return a finite score
    // (per-term ~MAX/4; summing 4 of them overflows f32 but stays finite in
    // f64, and the final result equals f32::MAX, i.e. representable).
    let big = f32::MAX.sqrt() / 2.0;
    let a = vec![big; 4];
    let b = vec![big; 4];
    let s = dot_f32(&a, &b).unwrap();
    assert!(s.is_finite(), "score must be finite, got {s}");
    assert!(
        (s - f32::MAX).abs() < f32::MAX * 1e-6,
        "expected ~f32::MAX, got {s}"
    );
    // Overflowing inputs (individual product > f32 range) are rejected, never
    // emitted as inf.
    let huge = vec![f32::MAX; 2];
    assert!(dot_f32(&huge, &huge).is_err());
    // non-finite inputs rejected
    assert!(dot_f32(&[f32::NAN, 1.0], &[1.0, 1.0]).is_err());
    assert!(dot_f32(&[f32::INFINITY, 0.0], &[0.0, 1.0]).is_err());
}

#[test]
fn dot_embedding_bytes_is_f64_safe_and_rejects_bad_input() {
    // dim == bytes/4; ordinary dot product exact.
    let bytes = f32_vec_to_bytes(&[1.0, 2.0, 3.0]);
    assert_eq!(
        dot_embedding_bytes(&[1.0, 2.0, 3.0], &bytes, 3).unwrap(),
        14.0
    );
    // dimension mismatch rejected (existing contract preserved).
    assert!(dot_embedding_bytes(&[1.0, 2.0, 3.0], &bytes, 2).is_err());
    assert!(dot_embedding_bytes(&[1.0; 13], &bytes, 3).is_err());
    // non-finite query rejected.
    let bad_query = vec![f32::NAN, 2.0, 3.0];
    assert!(dot_embedding_bytes(&bad_query, &bytes, 3).is_err());
    // non-finite stored bytes rejected (bit pattern of inf).
    let inf_bytes = f32_vec_to_bytes(&[1.0, f32::INFINITY, 1.0]);
    assert!(dot_embedding_bytes(&[1.0, 2.0, 3.0], &inf_bytes, 3).is_err());
    // Large finite f32 components: f64 accumulation keeps intermediate sum
    // finite even where f32 would saturate to inf, and the result (≈f32::MAX)
    // is still representable.
    let big = f32::MAX.sqrt() / 2.0f32.sqrt(); // per-term product ≈ MAX/2
    let big_bytes = f32_vec_to_bytes(&[big; 2]);
    let big_q = [big; 2];
    assert!(
        dot_embedding_bytes(&big_q, &big_bytes, 2)
            .unwrap()
            .is_finite()
    );
    // Even larger components that would overflow f32 are rejected outright.
    let huge_bytes = f32_vec_to_bytes(&[f32::MAX; 2]);
    let huge_q = [f32::MAX; 2];
    assert!(dot_embedding_bytes(&huge_q, &huge_bytes, 2).is_err());
}

#[test]
fn passage_top_k_matches_full_sort_with_ties_and_boundaries() {
    let make = |id: u128, score: f32| {
        (
            PassageHit {
                passage_id: uuid::Uuid::from_u128(id),
                section_id: uuid::Uuid::nil(),
                page_id: page_id(1),
                workspace_id: WorkspaceId::new(),
                project_id: ProjectId::new(),
                page_path: path("fixture.md"),
                page_title: "fixture".into(),
                heading_path: String::new(),
                text: "fixture".into(),
                start_byte: 0,
                end_byte: 7,
                rank: 0,
                lexical_rank: None,
                dense_rank: None,
                rrf_score: None,
            },
            score,
        )
    };
    let candidates = vec![
        make(9, 0.5),
        make(1, 0.5),
        make(7, 1.0),
        make(3, -0.2),
        make(2, 0.5),
    ];
    let mut full = candidates.clone();
    full.sort_by(|a, b| {
        dense_total_cmp(
            a.1,
            a.0.passage_id.as_bytes(),
            b.1,
            b.0.passage_id.as_bytes(),
        )
    });
    let ids = |items: &[(PassageHit, f32)]| {
        items
            .iter()
            .map(|(h, s)| (h.passage_id, s.to_bits()))
            .collect::<Vec<_>>()
    };
    for k in 0..=candidates.len() + 1 {
        let mut actual = candidates.clone();
        dense_passage_top_k(&mut actual, k);
        assert_eq!(ids(&actual), ids(&full[..k.min(full.len())]));
    }
}
