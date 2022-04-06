#![feature(hash_drain_filter)]
#![feature(drain_filter)]

extern crate petgraph;

use petgraph::dot::Dot;
use petgraph::prelude::{Incoming, NodeIndex};
use petgraph::visit::{depth_first_search, Control, DfsEvent};
use petgraph::Graph;
use std::collections::{HashMap, HashSet, LinkedList};

#[derive(Debug, PartialEq, Eq, Hash)]
struct JsModule<'a> {
    name: &'a str,
    size: usize,
}

#[derive(Debug)]
struct Dependency {
    is_async: bool,
}

#[derive(Debug, Default)]
struct Chunk {
    module_ids: Vec<NodeIndex>,
    size: usize,
    source_bundles: Vec<NodeIndex>,
}

impl Chunk {
    fn from_asset(asset_id: NodeIndex, asset: &JsModule) -> Self {
        Chunk {
            module_ids: vec![asset_id],
            size: asset.size,
            source_bundles: vec![],
        }
    }
}

fn main() {
    let (g, entries) = build_graph();
    println!("{:?}", Dot::new(&g));

    let mut chunk_roots = HashMap::new();
    let mut reachable_chunks = HashSet::new();
    let mut chunk_graph = Graph::new();

    // Step 1: Create chunks at the explicit split points in the graph.
    // Create chunks for each entry.
    for entry in &entries {
        let chunk_id = chunk_graph.add_node(Chunk::from_asset(*entry, &g[*entry]));
        chunk_roots.insert(*entry, (chunk_id, chunk_id));
    }

    // Traverse the module graph and create chunks for asset type changes and async dependencies.
    // This only adds the module asset of each chunk, not the subgraph.
    let mut stack = LinkedList::new();
    depth_first_search(&g, entries, |event| {
        match event {
            DfsEvent::Discover(asset_id, _) => {
                // Push to the stack when a new chunk is created.
                if let Some((_, chunk_group_id)) = chunk_roots.get(&asset_id) {
                    stack.push_front((asset_id, *chunk_group_id));
                }
            }
            DfsEvent::TreeEdge(importer_id, importee_id) => {
                // Create a new bundle as well as a new bundle group if the dependency is async.
                let dependency = &g[g.find_edge(importer_id, importee_id).unwrap()];
                if dependency.is_async {
                    let chunk_id =
                        chunk_graph.add_node(Chunk::from_asset(importee_id, &g[importee_id]));
                    chunk_roots.insert(importee_id, (chunk_id, chunk_id));

                    // Walk up the stack until we hit a different asset type
                    // and mark each this bundle as reachable from every parent bundle.
                    for (module_id, _) in &stack {
                        reachable_chunks.insert((*module_id, importee_id));
                    }
                }
            }
            DfsEvent::Finish(finished_module_id, _) => {
                // Pop the stack when existing the asset node that created a bundle.
                if let Some((module_id, _)) = stack.front() {
                    if *module_id == finished_module_id {
                        stack.pop_front();
                    }
                }
            }
            _ => {}
        }
    });

    println!("roots {:?}", chunk_roots);
    println!("reachable {:?}", reachable_chunks);
    println!("initial bundle graph {:?}", Dot::new(&chunk_graph));

    // Step 2: Determine reachability for every asset from each bundle root.
    // This is later used to determine which bundles to place each asset in.
    let mut reachable_nodes = HashSet::new();
    for (root, _) in &chunk_roots {
        depth_first_search(&g, Some(*root), |event| {
            if let DfsEvent::Discover(n, _) = &event {
                if n == root {
                    return Control::Continue;
                }

                // Stop when we hit another bundle root.
                if chunk_roots.contains_key(&n) {
                    return Control::<()>::Prune;
                }

                reachable_nodes.insert((*root, *n));
            }
            Control::Continue
        });
    }

    let reachable_graph = Graph::<(), ()>::from_edges(&reachable_nodes);
    println!("{:?}", Dot::new(&reachable_graph));

    // Step 3: Place all assets into bundles. Each asset is placed into a single
    // bundle based on the bundle entries it is reachable from. This creates a
    // maximally code split bundle graph with no duplication.

    // Create a mapping from entry asset ids to bundle ids.
    let mut bundles: HashMap<Vec<NodeIndex>, NodeIndex> = HashMap::new();

    for asset_id in g.node_indices() {
        // Find bundle entries reachable from the asset.
        let reachable: Vec<NodeIndex> = reachable_graph
            .neighbors_directed(asset_id, Incoming)
            .collect();

        // Filter out bundles when the asset is reachable in a parent bundle.
        let reachable: Vec<NodeIndex> = reachable
            .iter()
            .cloned()
            .filter(|b| {
                (&reachable)
                    .into_iter()
                    .all(|a| !reachable_chunks.contains(&(*a, *b)))
            })
            .collect();

        if let Some((bundle_id, _)) = chunk_roots.get(&asset_id) {
            // If the asset is a bundle root, add the bundle to every other reachable bundle group.
            bundles.entry(vec![asset_id]).or_insert(*bundle_id);
            for a in &reachable {
                if *a != asset_id {
                    chunk_graph.add_edge(chunk_roots[a].1, *bundle_id, 0);
                }
            }
        } else if reachable.len() > 0 {
            // If the asset is reachable from more than one entry, find or create
            // a bundle for that combination of entries, and add the asset to it.
            let source_bundles = reachable.iter().map(|a| bundles[&vec![*a]]).collect();
            let bundle_id = bundles.entry(reachable.clone()).or_insert_with(|| {
                let mut bundle = Chunk::default();
                bundle.source_bundles = source_bundles;
                chunk_graph.add_node(bundle)
            });

            let bundle = &mut chunk_graph[*bundle_id];
            bundle.module_ids.push(asset_id);
            bundle.size += g[asset_id].size;

            // Add the bundle to each reachable bundle group.
            for a in reachable {
                if a != *bundle_id {
                    chunk_graph.add_edge(chunk_roots[&a].1, *bundle_id, 0);
                }
            }
        }
    }

    // Step 4: Remove shared bundles that are smaller than the minimum size,
    // and add the assets to the original source bundles they were referenced from.
    // This may result in duplication of assets in multiple bundles.
    for bundle_id in chunk_graph.node_indices() {
        let bundle = &chunk_graph[bundle_id];
        if bundle.source_bundles.len() > 0 && bundle.size < 10 {
            remove_bundle(&g, &mut chunk_graph, bundle_id);
        }
    }

    // Step 5: Remove shared bundles from bundle groups that hit the parallel request limit.
    let limit = usize::MAX;
    for (_, (bundle_id, bundle_group_id)) in chunk_roots {
        // Only handle bundle group entries.
        if bundle_id != bundle_group_id {
            continue;
        }

        // Find the bundles in this bundle group.
        let mut neighbors: Vec<NodeIndex> = chunk_graph.neighbors(bundle_group_id).collect();
        if neighbors.len() > limit {
            // Sort the bundles so the smallest ones are removed first.
            neighbors.sort_by(|a, b| chunk_graph[*a].size.cmp(&chunk_graph[*b].size));

            // Remove bundles until the bundle group is within the parallel request limit.
            for bundle_id in &neighbors[0..neighbors.len() - limit] {
                // Add all assets in the shared bundle into the source bundles that are within this bundle group.
                let source_bundles: Vec<NodeIndex> = chunk_graph[*bundle_id]
                    .source_bundles
                    .drain_filter(|s| neighbors.contains(s))
                    .collect();
                for source in source_bundles {
                    for asset_id in chunk_graph[*bundle_id].module_ids.clone() {
                        let bundle_id = bundles[&vec![source]];
                        let bundle = &mut chunk_graph[bundle_id];
                        bundle.module_ids.push(asset_id);
                        bundle.size += g[asset_id].size;
                    }
                }

                // Remove the edge from this bundle group to the shared bundle.
                chunk_graph
                    .remove_edge(chunk_graph.find_edge(bundle_group_id, *bundle_id).unwrap());

                // If there is now only a single bundle group that contains this bundle,
                // merge it into the remaining source bundles. If it is orphaned entirely, remove it.
                let count = chunk_graph.neighbors_directed(*bundle_id, Incoming).count();
                if count == 1 {
                    remove_bundle(&g, &mut chunk_graph, *bundle_id);
                } else if count == 0 {
                    chunk_graph.remove_node(*bundle_id);
                }
            }
        }
    }

    println!("chunk graph {:?}", Dot::new(&chunk_graph));

    for bundle_id in chunk_graph.node_indices() {
        let chunk = &chunk_graph[bundle_id];
        println!(
            "{:?} {} {}",
            bundle_id,
            chunk
                .module_ids
                .iter()
                .map(|n| g[*n].name)
                .collect::<Vec<&str>>()
                .join(", "),
            chunk.size
        )
    }
}

fn remove_bundle(
    asset_graph: &Graph<JsModule, Dependency>,
    bundle_graph: &mut Graph<Chunk, i32>,
    bundle_id: NodeIndex,
) {
    let bundle = bundle_graph.remove_node(bundle_id).unwrap();
    for asset_id in &bundle.module_ids {
        for source_bundle_id in &bundle.source_bundles {
            let bundle = &mut bundle_graph[*source_bundle_id];
            bundle.module_ids.push(*asset_id);
            bundle.size += asset_graph[*asset_id].size;
        }
    }
}

fn build_graph<'a>() -> (Graph<JsModule<'a>, Dependency>, Vec<NodeIndex>) {
    let mut g = Graph::new();
    let mut entries = Vec::new();

    let entry_a_js = g.add_node(JsModule {
        name: "entry-a.js",
        size: 1000,
    });

    let entry_b_js = g.add_node(JsModule {
        name: "entry-b.js",
        size: 1000,
    });

    let a_js = g.add_node(JsModule {
        name: "a.js",
        size: 1000,
    });
    let b_js = g.add_node(JsModule {
        name: "b.js",
        size: 1000,
    });

    let shared_js = g.add_node(JsModule {
        name: "shared.js",
        size: 1000,
    });

    let asynced_a_js = g.add_node(JsModule {
        name: "asynced_a.js",
        size: 1000,
    });

    g.add_edge(entry_a_js, a_js, Dependency { is_async: false });
    g.add_edge(entry_a_js, asynced_a_js, Dependency { is_async: true });
    g.add_edge(entry_a_js, shared_js, Dependency { is_async: false });
    g.add_edge(entry_b_js, b_js, Dependency { is_async: false });
    g.add_edge(entry_b_js, shared_js, Dependency { is_async: false });

    entries.push(entry_a_js);
    entries.push(entry_b_js);

    return (g, entries);
}
