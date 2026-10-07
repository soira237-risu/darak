//! Run under native Windows for performance evidence; Wine runs are diagnostic only.
use darak_core::perf::{self, Shape};
use darak_core::search::{Query, search};
use darak_core::{Kind, SizeMode, Tree};
use std::time::Instant;

fn main() -> Result<(), String> {
    let (mut nodes, mut shape, mut unicode, mut threads, mut iterations) =
        (100000, Shape::Balanced, 25, 4, 30);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--nodes" => nodes = value.parse().map_err(|_| "invalid nodes")?,
            "--shape" => {
                shape = match value.as_str() {
                    "wide" => Shape::Wide,
                    "deep" => Shape::Deep,
                    "balanced" => Shape::Balanced,
                    _ => return Err("invalid shape".into()),
                }
            }
            "--unicode-percent" => {
                unicode = value.parse().map_err(|_| "invalid Unicode percentage")?
            }
            "--threads" => threads = value.parse().map_err(|_| "invalid threads")?,
            "--iterations" => iterations = value.parse().map_err(|_| "invalid iterations")?,
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if nodes == 0 || nodes >= u32::MAX as usize || unicode > 100 || threads == 0 || iterations == 0
    {
        return Err("invalid argument range".into());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| e.to_string())?;
    let tree = perf::make_fixture(nodes, shape, unicode);
    validate(&tree)?;
    let shape_name = match shape {
        Shape::Wide => "wide",
        Shape::Deep => "deep",
        Shape::Balanced => "balanced",
    };
    let fixture = format!(
        "seed{}-{shape_name}-n{nodes}-u{unicode}-{:016x}",
        perf::FIXTURE_SEED,
        perf::fingerprint(&tree)
    );
    let focus = tree
        .children(0)
        .find(|&id| tree.node(id).kind == Kind::Dir)
        .unwrap_or(0);
    let matrix = [
        ("all", "*"),
        ("contains-broad", "node"),
        ("contains-few", "node_00000001"),
        ("no-hit", "absent_20261007"),
        ("exclude", "node !000000"),
        ("unicode", "자료"),
        ("unicode-case", "i̇"),
        ("wildcard", "node_????????*.txt"),
        ("ext", "ext:txt;rs"),
        ("size", "size:>16mb"),
        ("date", "dm:<7d"),
        ("files", "file:"),
        ("dirs", "folder:"),
        ("path", "path:node_00000001"),
        (
            "multi",
            "file: ext:txt;log size:>1mb dm:<30d !999 path:fixture",
        ),
    ];
    println!("schema,fixture,query,iteration,nodes,hits,total_size,elapsed_us,result_fingerprint");
    eprintln!("schema,fixture,query,scope,mode,raw_query");
    for (scope_name, scope) in [("root", 0), ("focus", focus)] {
        // Validate scope outside timings with one BFS pass, including deep corpora.
        let mut in_scope = vec![false; tree.len()];
        in_scope[scope as usize] = true;
        for id in 1..tree.len() {
            in_scope[id] |= in_scope[tree.nodes[id].parent as usize];
        }
        for (name, text) in matrix {
            let query_id = format!("{name}-{scope_name}");
            eprintln!("1,{fixture},{query_id},{scope},allocated,{text}");
            let q = Query::parse(text, perf::FIXTURE_NOW_SECS2000)?;
            for _ in 0..3 {
                pool.install(|| search(&tree, &q, SizeMode::Allocated, scope, usize::MAX));
            }
            let mut expected = None;
            for iteration in 1..=iterations {
                let start = Instant::now();
                let result =
                    pool.install(|| search(&tree, &q, SizeMode::Allocated, scope, usize::MAX));
                let elapsed = start.elapsed();
                if result
                    .hits
                    .iter()
                    .any(|&id| id as usize >= tree.len() || !in_scope[id as usize])
                {
                    return Err("out-of-scope result".into());
                }
                let fingerprint = perf::result_fingerprint(&result);
                if expected.is_some_and(|e| e != fingerprint) {
                    return Err(format!("unstable result fingerprint: {query_id}"));
                }
                expected = Some(fingerprint);
                println!(
                    "1,{fixture},{query_id},{iteration},{nodes},{},{},{},{fingerprint}",
                    result.total,
                    result.total_size,
                    elapsed.as_micros()
                );
            }
        }
    }
    Ok(())
}

fn validate(tree: &Tree) -> Result<(), String> {
    let mut seen = vec![false; tree.len()];
    let mut name_end = 0;
    let mut depth = vec![0usize; tree.len()];
    for (id, n) in tree.nodes.iter().enumerate() {
        if id > 0 {
            if n.parent as usize >= id {
                return Err("parent-before-child invariant".into());
            }
            depth[id] = depth[n.parent as usize] + 1;
            if depth[id] < depth[id - 1] || depth[id] > 1000 {
                return Err("BFS/depth invariant".into());
            }
        }
        if n.name_off as usize != name_end {
            return Err("name pool ordering invariant".into());
        }
        name_end += n.name_len as usize;
        if !tree.names.is_char_boundary(name_end) {
            return Err("name range invariant".into());
        }
        for child in tree.children(id as u32) {
            if child as usize >= tree.len()
                || tree.node(child).parent != id as u32
                || seen[child as usize]
            {
                return Err("child range invariant".into());
            }
            seen[child as usize] = true;
        }
    }
    if name_end != tree.names.len() || seen[1..].iter().any(|&x| !x) {
        return Err("unowned fixture node/name".into());
    }
    Ok(())
}
