// Build script: train the v1 zstd dictionary for tree objects and embed it
// as `tree_dict_v1.bin` in OUT_DIR so it can be included via `include_bytes!`.
//
// The dictionary is trained from representative tree JSON samples that cover
// typical omemfs repository structures. The samples use the raw serialised form
// (ED F1 prefix + minimised JSON) to match exactly what the compress stage receives.

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dict_path = out_dir.join("tree_dict_v1.bin");

    let samples = make_samples();

    let dict = zstd::dict::from_samples(&samples, 32 * 1024).expect("dictionary training failed");

    let mut f = std::fs::File::create(&dict_path).expect("failed to create dict file");
    f.write_all(&dict).expect("failed to write dict");

    println!("cargo:rerun-if-changed=build.rs");
}

// Produce representative tree JSON samples with the ED F1 prefix.
// Covers: single blob, mixed blob+tree, executable blob, large trees,
// deeply nested structures, symlinks, empty trees, and various mtimes/sizes.
fn make_samples() -> Vec<Vec<u8>> {
    // Helper to prepend the ED F1 type tag (same as Tree::serialise).
    let tag_tree = |json: &str| -> Vec<u8> {
        let mut v = vec![0xED_u8, 0xF1];
        v.extend_from_slice(json.as_bytes());
        v
    };

    let mut samples = Vec::new();

    // Single-entry trees — blob.
    for (name, ext) in &[
        ("README", "md"),
        ("main", "rs"),
        ("lib", "rs"),
        ("mod", "rs"),
        ("config", "json"),
        ("Cargo", "toml"),
        ("build", "rs"),
        ("error", "rs"),
        ("object", "rs"),
        ("repo", "rs"),
        ("scan", "rs"),
        ("stub", "rs"),
        ("tree_ops", "rs"),
    ] {
        let hash = fake_hash(name);
        let json = format!(
            r#"{{"kind":"normal","entries":[{{"hash":"{hash}","kind":"blob","mtime":"2026-05-10T08:30:00Z","name":"{name}.{ext}","size":1234}}]}}"#,
        );
        samples.push(tag_tree(&json));
    }

    // Trees with multiple blobs.
    for n in [2usize, 3, 5, 8, 10, 15, 20] {
        let entries: String = (0..n)
            .map(|i| {
                let hash = fake_hash(&format!("file{i:04}"));
                format!(
                    r#"{{"hash":"{hash}","kind":"blob","mtime":"2026-04-01T12:00:00Z","name":"file{i:04}.txt","size":{size}}}"#,
                    size = 100 + i * 50
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"kind":"normal","entries":[{entries}]}}"#);
        samples.push(tag_tree(&json));
    }

    // Trees with sub-directories.
    for (dir_name, dir_hash) in &[
        ("src", "aabb"),
        ("tests", "ccdd"),
        ("design", "eeff"),
        ("docs", "1122"),
        ("target", "3344"),
    ] {
        let bh = fake_hash(dir_name);
        let json = format!(
            r#"{{"kind":"normal","entries":[{{"hash":"{bh}","kind":"blob","mtime":"2026-05-11T10:00:00Z","name":"README.md","size":512}},{{"hash":"{dir_hash}000000000000000000000000000000000000000000000000000000000000","kind":"tree","mtime":"2026-05-11T10:00:00Z","name":"{dir_name}","size":8192}}]}}"#,
        );
        samples.push(tag_tree(&json));
    }

    // Executable blob (mode field present).
    for name in &["run.sh", "build.sh", "deploy.sh", "setup.sh"] {
        let hash = fake_hash(name);
        let json = format!(
            r#"{{"kind":"normal","entries":[{{"hash":"{hash}","kind":"blob","mode":"755","mtime":"2026-03-15T09:00:00Z","name":"{name}","size":128}}]}}"#,
        );
        samples.push(tag_tree(&json));
    }

    // Symlink entries.
    let json = r#"{"kind":"normal","entries":[{"kind":"symlink","mtime":"2026-05-01T00:00:00Z","name":"link","size":0,"target":"../other"},{"hash":"abcd000000000000000000000000000000000000000000000000000000001234","kind":"blob","mtime":"2026-05-01T00:00:00Z","name":"real.txt","size":64}]}"#;
    samples.push(tag_tree(json));

    // Empty tree.
    samples.push(tag_tree(r#"{"kind":"normal","entries":[]}"#));

    // Large directory with many files — maximally repetitive field names.
    let entries: String = (0..50)
        .map(|i| {
            let hash = fake_hash(&format!("big{i}"));
            format!(
                r#"{{"hash":"{hash}","kind":"blob","mtime":"2026-01-{:02}T00:00:00Z","name":"document_{i:04}.md","size":{}}}"#,
                (i % 28) + 1,
                1000 + i * 37
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r#"{{"kind":"normal","entries":[{entries}]}}"#);
    samples.push(tag_tree(&json));

    // Mix: blobs, trees, symlinks.
    let json = r#"{"kind":"normal","entries":[{"hash":"1111000000000000000000000000000000000000000000000000000000001111","kind":"blob","mtime":"2026-05-16T10:00:00Z","name":"Cargo.toml","size":800},{"hash":"2222000000000000000000000000000000000000000000000000000000002222","kind":"tree","mtime":"2026-05-16T10:00:00Z","name":"src","size":15000},{"hash":"3333000000000000000000000000000000000000000000000000000000003333","kind":"tree","mtime":"2026-05-14T08:00:00Z","name":"tests","size":6000},{"kind":"symlink","mtime":"2026-05-10T00:00:00Z","name":"README.md","size":0,"target":"docs/README.md"}]}"#;
    samples.push(tag_tree(json));

    // Vary mtime formats and null mtime.
    let hash = fake_hash("notime");
    let json = format!(
        r#"{{"kind":"normal","entries":[{{"hash":"{hash}","kind":"blob","mtime":null,"name":"notime.bin","size":4096}}]}}"#,
    );
    samples.push(tag_tree(&json));

    // Duplicate samples to increase training weight (small-file dictionary training
    // requires many samples; more repetitions → better compression).
    let base_len = samples.len();
    for _ in 0..7 {
        for i in 0..base_len {
            samples.push(samples[i].clone());
        }
    }

    samples
}

// Produce a syntactically valid 64-hex-char hash from a short seed string.
fn fake_hash(seed: &str) -> String {
    let mut h = 0u64;
    for &b in seed.as_bytes() {
        h = h
            .wrapping_mul(6364136223846793005)
            .wrapping_add(b as u64 + 1442695040888963407);
    }
    // Produce 64 hex chars from two 32-char halves.
    format!(
        "{:016x}{:016x}{:016x}{:016x}",
        h,
        h ^ 0xDEADBEEF_CAFEBABE,
        h.wrapping_add(1),
        h ^ 0x0123456789ABCDEF
    )
}
