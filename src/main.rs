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
    fn from_js_module(module_idx: NodeIndex, module: &JsModule) -> Self {
        Chunk {
            module_ids: vec![module_idx],
            size: module.size,
            source_bundles: vec![],
        }
    }
}

fn main() {
    let (g, entries) = build_graph();
    println!("{:?}", Dot::new(&g));


    // 存的是 chunk 的入口模块的 id 和对应的 chunk id组成的元组
    let mut chunk_roots = HashMap::new();
    let mut reachable_chunks = HashSet::new();
    let mut chunk_graph = Graph::new();

    // Step 1: Create chunks at the explicit split points in the graph.
    // Create chunks for each entry.
    for entry in &entries {
        let chunk_id = chunk_graph.add_node(Chunk::from_js_module(*entry, &g[*entry]));
        chunk_roots.insert(*entry, (chunk_id, chunk_id));
    }

    // Traverse the module graph and create chunks for async dependencies or other condition.
    // This only adds the module asset of each chunk, not the subgraph.
    // stack 的队头表示的当前 chunk 入口模块的 图索引 和其所属的 chunk 的 id
    // stack 的 n + 1 位置的 chunk 是 n 的父 chunk ，即 chunk (n) import 了 chunk (n + 1)
    let mut stack = LinkedList::new();
    depth_first_search(&g, entries, |event| {
        match event {
            DfsEvent::Discover(module_idx, _) => {
                // println!("Discover {:?}", module_idx);
                // Push to the stack when a new chunk is created.
                if let Some((_, chunk_group_id)) = chunk_roots.get(&module_idx) {
                    // stack 的队头表示的 chunk 入口模块的 图索引 和其所属的 chunk 的 id
                    stack.push_front((module_idx, *chunk_group_id));
                }
            }
            DfsEvent::TreeEdge(importer_id, importee_id) => {
                // println!("TreeEdge from {:?} to {:?}", importer_id, importee_id);
                // Create a new bundle as well as a new bundle group if the dependency is async.
                let dependency = &g[g.find_edge(importer_id, importee_id).unwrap()];
                if dependency.is_async {
                    let chunk = Chunk::from_js_module(importee_id, &g[importee_id]);
                    let chunk_id = chunk_graph.add_node(chunk);
                    chunk_roots.insert(importee_id, (chunk_id, chunk_id));

                    // Walk up the stack until we hit a different asset type
                    // and mark each this bundle as reachable from every parent bundle.
                    for (chunk_entry_module_idx, _) in &stack {
                        reachable_chunks.insert((*chunk_entry_module_idx, importee_id));
                    }
                }
            }
            DfsEvent::Finish(finished_module_id, _) => {
              // println!("Finish {:?}", finished_module_id);
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
    // chunk_roots 
    println!("roots {:#?}", chunk_roots);
    // reachable 存储着 entry chunk module 到各个 chunk entry module 之间的边，不存在说明对应模块不可达
    println!("reachable_chunks {:?}", reachable_chunks);
    println!("initial chunk graph {:?}", Dot::new(&chunk_graph));
    // 此时 chunk_graph 中的每一个 chunk 仅包含自己的入口模块

    // Step 2: Determine reachability for every module from each chunk root.
    // This is later used to determine which chunk to place each module in.
    let mut reachable_modules = HashSet::new();

    for (root_which_is_node_idx_of_chunks_entry_module, _) in &chunk_roots {
        depth_first_search(&g, Some(*root_which_is_node_idx_of_chunks_entry_module), |event| {
            if let DfsEvent::Discover(node_idx_of_visiting_module, _) = &event {
                if node_idx_of_visiting_module == root_which_is_node_idx_of_chunks_entry_module {
                    return Control::Continue;
                }

                reachable_modules.insert((*root_which_is_node_idx_of_chunks_entry_module, *node_idx_of_visiting_module));

                 // Stop when we hit another bundle root.
                 if chunk_roots.contains_key(&node_idx_of_visiting_module) {
                  return Control::<()>::Prune;
              }
            }
            Control::Continue
        });
    }

    let reachable_module_graph = Graph::<(), ()>::from_edges(&reachable_modules);
    println!("reachable_module_graph {:?}", Dot::new(&reachable_module_graph));

    // Step 3: Place all modules into chunks. Each module is placed into a single
    // chunk based on the chunk entries it is reachable from. This creates a
    // maximally code split chunk graph with no duplication.

    // Create a mapping from entry module ids to chunk ids.
    let mut chunks: HashMap<Vec<NodeIndex>, NodeIndex> = HashMap::new();

    for module_id in g.node_indices() {
        // Find chunk entries reachable from the module.
        let reachable: Vec<NodeIndex> = reachable_module_graph
            .neighbors_directed(module_id, Incoming)
            .collect();
        println!("original reachable: {:?} for {:?}", reachable, module_id);
        // Filter out chunks when the module is reachable in a parent chunk.
        let reachable: Vec<NodeIndex> = reachable
            .iter()
            .cloned()
            .filter(|b| {
                (&reachable)
                    .into_iter()
                    .all(|a| !reachable_chunks.contains(&(*a, *b)))
            })
            .collect();

          println!("filtered reachable: {:?}", reachable);

        if let Some((chunk_id, _)) = chunk_roots.get(&module_id) {
            // If the module is a chunk root, add the chunk to every other reachable chunk group.
            chunks.entry(vec![module_id]).or_insert(*chunk_id);
            for a in &reachable {
                if *a != module_id {
                    chunk_graph.add_edge(chunk_roots[a].1, *chunk_id, 0);
                }
            }
        } else if reachable.len() > 0 {
            // If the asset is reachable from more than one entry, find or create
            // a chunk for that combination of entries, and add the asset to it.
            let source_chunks = reachable.iter().map(|a| chunks[&vec![*a]]).collect();
            // 这里创建了共享模块的 chunk
            let chunk_id = chunks.entry(reachable.clone()).or_insert_with(|| {
                let mut bundle = Chunk::default();
                bundle.source_bundles = source_chunks;
                chunk_graph.add_node(bundle)
            });

            let bundle = &mut chunk_graph[*chunk_id];
            bundle.module_ids.push(module_id);
            bundle.size += g[module_id].size;

            // Add the bundle to each reachable bundle group.
            for a in reachable {
                if a != *chunk_id {
                    chunk_graph.add_edge(chunk_roots[&a].1, *chunk_id, 0);
                }
            }
        }
    }

        println!("chunk_graph in step3: {:#?}", Dot::new(&chunk_graph));

    // Step 4: Remove shared bundles that are smaller than the minimum size,
    // and add the assets to the original source bundles they were referenced from.
    // This may result in duplication of assets in multiple bundles.
    for bundle_id in chunk_graph.node_indices() {
        let bundle = &chunk_graph[bundle_id];
        if bundle.source_bundles.len() > 0 && bundle.size < 10 {
            remove_bundle(&g, &mut chunk_graph, bundle_id);
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
    // g.add_edge(entry_b_js, asynced_a_js, Dependency { is_async: true });
    g.add_edge(entry_b_js, shared_js, Dependency { is_async: false });

    entries.push(entry_a_js);
    entries.push(entry_b_js);

    return (g, entries);
}
