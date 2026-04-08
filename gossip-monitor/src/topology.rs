use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct PeerSuggestion {
    pub target_node_id: String,
    pub suggested_peers: Vec<String>,
    pub reason: String,
    pub isolation_score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopologyAnalysis {
    pub suggestions: Vec<PeerSuggestion>,
    pub node_betweenness: Vec<(String, f64)>,
    pub timestamp: u64,
}

/// BFS from `start`, marking visited nodes. Returns all nodes in this component.
pub fn bfs_component(
    start: &str,
    adj: &HashMap<String, HashSet<String>>,
    visited: &mut HashSet<String>,
) -> Vec<String> {
    let mut component = Vec::new();
    let mut queue = VecDeque::new();
    visited.insert(start.to_string());
    queue.push_back(start.to_string());
    while let Some(node) = queue.pop_front() {
        component.push(node.clone());
        if let Some(neighbors) = adj.get(&node) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    visited.insert(neighbor.clone());
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }
    component
}

/// Brandes' algorithm for betweenness centrality on an undirected graph.
pub fn compute_betweenness(
    nodes: &[String],
    adj: &HashMap<String, HashSet<String>>,
) -> HashMap<String, f64> {
    let mut betweenness: HashMap<String, f64> = HashMap::new();
    for node in nodes {
        betweenness.insert(node.clone(), 0.0);
    }

    for s in nodes {
        // BFS from s
        let mut stack: Vec<String> = Vec::new();
        let mut predecessors: HashMap<String, Vec<String>> = HashMap::new();
        let mut sigma: HashMap<String, f64> = HashMap::new();
        let mut dist: HashMap<String, i64> = HashMap::new();

        for node in nodes {
            predecessors.insert(node.clone(), Vec::new());
            sigma.insert(node.clone(), 0.0);
            dist.insert(node.clone(), -1);
        }
        sigma.insert(s.clone(), 1.0);
        dist.insert(s.clone(), 0);

        let mut queue = VecDeque::new();
        queue.push_back(s.clone());

        while let Some(v) = queue.pop_front() {
            stack.push(v.clone());
            let d_v = dist[&v];
            if let Some(neighbors) = adj.get(&v) {
                for w in neighbors {
                    // w found for the first time?
                    if dist[w] < 0 {
                        dist.insert(w.clone(), d_v + 1);
                        queue.push_back(w.clone());
                    }
                    // shortest path to w via v?
                    if dist[w] == d_v + 1 {
                        *sigma.get_mut(w).unwrap() += sigma[&v];
                        predecessors.get_mut(w).unwrap().push(v.clone());
                    }
                }
            }
        }

        // Back-propagation
        let mut delta: HashMap<String, f64> = HashMap::new();
        for node in nodes {
            delta.insert(node.clone(), 0.0);
        }

        while let Some(w) = stack.pop() {
            for v in &predecessors[&w] {
                let d = (sigma[v] / sigma[&w]) * (1.0 + delta[&w]);
                *delta.get_mut(v).unwrap() += d;
            }
            if w != *s {
                *betweenness.get_mut(&w).unwrap() += delta[&w];
            }
        }
    }

    // Divide by 2 for undirected graph
    for val in betweenness.values_mut() {
        *val /= 2.0;
    }

    betweenness
}

/// Return the top `count` nodes by betweenness, excluding any in `exclude`.
pub fn top_betweenness(
    betweenness: &HashMap<String, f64>,
    exclude: &HashSet<String>,
    count: usize,
) -> Vec<String> {
    let mut candidates: Vec<(&String, &f64)> = betweenness
        .iter()
        .filter(|(k, _)| !exclude.contains(*k))
        .collect();
    candidates.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    candidates
        .into_iter()
        .take(count)
        .map(|(k, _)| k.clone())
        .collect()
}

pub fn analyze_topology(snapshot: &crate::swarm::SwarmSnapshot) -> TopologyAnalysis {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Step 1: Build adjacency map from non-stale peers only
    let active_ids: HashSet<String> = snapshot
        .peers
        .iter()
        .filter(|p| !p.stale)
        .map(|p| p.node_id.clone())
        .collect();

    let mut adj: HashMap<String, HashSet<String>> = HashMap::new();
    for peer in snapshot.peers.iter().filter(|p| !p.stale) {
        let neighbors: HashSet<String> = peer
            .neighbors
            .iter()
            .filter(|n| active_ids.contains(*n))
            .cloned()
            .collect();
        // Ensure symmetric edges
        for n in &neighbors {
            adj.entry(n.clone())
                .or_default()
                .insert(peer.node_id.clone());
        }
        adj.entry(peer.node_id.clone())
            .or_default()
            .extend(neighbors);
    }

    let nodes: Vec<String> = active_ids.iter().cloned().collect();

    // Edge case: empty graph
    if nodes.is_empty() {
        return TopologyAnalysis {
            suggestions: Vec::new(),
            node_betweenness: Vec::new(),
            timestamp: now,
        };
    }

    // Step 2: Find connected components via BFS
    let mut visited: HashSet<String> = HashSet::new();
    let mut components: Vec<Vec<String>> = Vec::new();
    for node in &nodes {
        if !visited.contains(node) {
            let component = bfs_component(node, &adj, &mut visited);
            components.push(component);
        }
    }

    // Identify the largest component
    let largest_idx = components
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| c.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let main_component: HashSet<String> = components[largest_idx].iter().cloned().collect();

    // Step 3: Compute betweenness centrality
    let betweenness = compute_betweenness(&nodes, &adj);

    // Sorted betweenness for the result
    let mut node_betweenness: Vec<(String, f64)> = betweenness
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    node_betweenness.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let total_known = nodes.len();
    let mut suggestions: Vec<PeerSuggestion> = Vec::new();

    // Step 4: Nodes in disconnected components (not the largest)
    for (i, component) in components.iter().enumerate() {
        if i == largest_idx {
            continue;
        }
        let exclude: HashSet<String> = component.iter().cloned().collect();
        // Suggest top 3 from main component
        let main_betweenness: HashMap<String, f64> = betweenness
            .iter()
            .filter(|(k, _)| main_component.contains(*k))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let suggested = top_betweenness(&main_betweenness, &exclude, 3);
        for node_id in component {
            suggestions.push(PeerSuggestion {
                target_node_id: node_id.clone(),
                suggested_peers: suggested.clone(),
                reason: "isolated_clique".to_string(),
                isolation_score: 1.0,
            });
        }
    }

    // Step 5: Nodes in the main component
    for node_id in &components[largest_idx] {
        let neighbors = adj.get(node_id).cloned().unwrap_or_default();
        let neighbor_count = neighbors.len();

        if neighbor_count < 2 {
            // Low connectivity
            let mut exclude_set: HashSet<String> = neighbors.clone();
            exclude_set.insert(node_id.clone());
            let suggested = top_betweenness(&betweenness, &exclude_set, 3);
            suggestions.push(PeerSuggestion {
                target_node_id: node_id.clone(),
                suggested_peers: suggested,
                reason: "low_connectivity".to_string(),
                isolation_score: if neighbor_count == 0 { 1.0 } else { 0.5 },
            });
            continue;
        }

        // Compute cluster_density: edges among neighbors / max possible
        let neighbor_list: Vec<&String> = neighbors.iter().collect();
        let mut edges_among_neighbors: usize = 0;
        for i in 0..neighbor_list.len() {
            for j in (i + 1)..neighbor_list.len() {
                if let Some(adj_set) = adj.get(neighbor_list[i]) {
                    if adj_set.contains(neighbor_list[j]) {
                        edges_among_neighbors += 1;
                    }
                }
            }
        }
        let max_edges = neighbor_count * (neighbor_count - 1) / 2;
        let cluster_density = if max_edges > 0 {
            edges_among_neighbors as f64 / max_edges as f64
        } else {
            0.0
        };

        // Compute external_reach: 2-hop reachable nodes (excluding self and direct neighbors)
        let mut two_hop: HashSet<String> = HashSet::new();
        for neighbor in &neighbors {
            if let Some(second_neighbors) = adj.get(neighbor) {
                for sn in second_neighbors {
                    if sn != node_id && !neighbors.contains(sn) {
                        two_hop.insert(sn.clone());
                    }
                }
            }
        }
        let external_reach = if total_known > 1 {
            two_hop.len() as f64 / (total_known - 1) as f64
        } else {
            0.0
        };

        let isolation_score = cluster_density * (1.0 - external_reach);

        if cluster_density > 0.8 && external_reach < 0.3 {
            let mut exclude_set: HashSet<String> = neighbors.clone();
            exclude_set.insert(node_id.clone());
            let suggested = top_betweenness(&betweenness, &exclude_set, 3);
            suggestions.push(PeerSuggestion {
                target_node_id: node_id.clone(),
                suggested_peers: suggested,
                reason: "isolated_clique".to_string(),
                isolation_score,
            });
        }
    }

    // Step 6: Sort suggestions by isolation_score descending
    suggestions.sort_by(|a, b| {
        b.isolation_score
            .partial_cmp(&a.isolation_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    TopologyAnalysis {
        suggestions,
        node_betweenness,
        timestamp: now,
    }
}
