// Podping Gossip Monitor - Frontend
"use strict";

let currentData = null;
let simulation = null;

// Map of node_id -> Chart.js instance for peer cards
const chartInstances = new Map();

// ---- Helpers ----

function formatUptime(secs) {
    if (secs == null) return "-";
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    return h + "h " + m + "m";
}

function esc(s) {
    if (s == null) return "";
    const div = document.createElement("div");
    div.appendChild(document.createTextNode(s));
    return div.innerHTML;
}

function shortId(id) {
    if (!id) return "";
    return id.length > 12 ? id.substring(0, 12) + "\u2026" : id;
}

// ---- Version fetch ----

async function fetchVersion() {
    try {
        const resp = await fetch("/api/version");
        if (resp.ok) {
            const data = await resp.json();
            const el = document.getElementById("header-title");
            if (el && data.version) {
                el.textContent = "Podping Gossip Monitor v" + data.version;
            }
        }
    } catch (e) {
        console.error("Failed to fetch version:", e);
    }
}

// ---- Rendering ----

function renderSummary(data) {
    document.getElementById("stat-total").textContent = data.total_peers;
    document.getElementById("stat-active").textContent = data.active_peers;
    document.getElementById("stat-stale").textContent = data.stale_peers;
    document.getElementById("stat-edges").textContent = data.edges.length;
}

function renderTable(data) {
    const tbody = document.getElementById("peer-tbody");
    let html = "";
    for (const p of data.peers) {
        const cls = p.stale ? ' class="stale"' : "";
        const name = esc(p.friendly_name || shortId(p.node_id));
        html += "<tr" + cls + ">"
            + "<td>" + name + "</td>"
            + "<td>" + esc(p.version) + "</td>"
            + "<td>" + (p.cpu_percent != null ? p.cpu_percent.toFixed(1) + "%" : "-") + "</td>"
            + "<td>" + (p.memory_mb != null ? p.memory_mb + " MB" : "-") + "</td>"
            + "<td>" + (p.thread_count != null ? p.thread_count : "-") + "</td>"
            + "<td>" + (p.neighbor_count != null ? p.neighbor_count : "-") + "</td>"
            + "<td>" + formatUptime(p.uptime_secs) + "</td>"
            + "<td>" + (p.msgs_received != null ? p.msgs_received : "-") + "</td>"
            + "<td>" + (p.msgs_sent != null ? p.msgs_sent : "-") + "</td>"
            + "<td>" + (p.last_msg_age_secs != null ? p.last_msg_age_secs + "s" : "-") + "</td>"
            + "<td>" + (p.reconnect_count != null ? p.reconnect_count : "-") + "</td>"
            + "<td>" + esc(p.os) + "</td>"
            + "</tr>";
    }
    tbody.innerHTML = html;
}

// ---- Peer Cards ----

function buildTimeLabels(history, nowTs) {
    return history.map(function(sample) {
        var ago = Math.max(0, nowTs - sample.timestamp);
        var mins = Math.round(ago / 60);
        if (mins < 1) return "now";
        return mins + "m ago";
    });
}

function renderCards(data) {
    var container = document.getElementById("peer-cards");
    var nowTs = data.timestamp || Math.floor(Date.now() / 1000);

    // Track which peers are still present
    var activePeerIds = new Set();

    for (var i = 0; i < data.peers.length; i++) {
        var p = data.peers[i];
        activePeerIds.add(p.node_id);

        var cardId = "card-" + p.node_id;
        var card = document.getElementById(cardId);
        var canvasId = "chart-" + p.node_id;

        if (!card) {
            // Create new card
            card = document.createElement("div");
            card.id = cardId;
            card.className = "peer-card" + (p.stale ? " stale" : "");
            card.innerHTML =
                '<div class="card-header">' +
                    '<span class="peer-name"></span>' +
                    '<span class="version-badge"></span>' +
                    '<span class="status-dot"></span>' +
                '</div>' +
                '<div class="card-stats"></div>' +
                '<div class="card-chart"><canvas id="' + canvasId + '"></canvas></div>' +
                '<div class="card-footer"></div>';
            container.appendChild(card);
        }

        // Update card class
        card.className = "peer-card" + (p.stale ? " stale" : "");

        // Header
        var nameEl = card.querySelector(".peer-name");
        nameEl.textContent = p.friendly_name || shortId(p.node_id);
        nameEl.title = p.node_id;

        card.querySelector(".version-badge").textContent = p.version || "-";

        var dot = card.querySelector(".status-dot");
        dot.className = "status-dot" + (p.stale ? " stale" : "");

        // Stats
        card.querySelector(".card-stats").innerHTML =
            "<span>CPU: <strong>" + (p.cpu_percent != null ? p.cpu_percent.toFixed(1) + "%" : "-") + "</strong></span>" +
            "<span>Mem: <strong>" + (p.memory_mb != null ? p.memory_mb + " MB" : "-") + "</strong></span>" +
            "<span>Thr: <strong>" + (p.thread_count != null ? p.thread_count : "-") + "</strong></span>" +
            "<span>Nbr: <strong>" + (p.neighbor_count != null ? p.neighbor_count : "-") + "</strong></span>" +
            "<span>Up: <strong>" + formatUptime(p.uptime_secs) + "</strong></span>";

        // Footer
        var osBuild = [];
        if (p.os) osBuild.push(p.os);
        if (p.build_type) osBuild.push(p.build_type);
        card.querySelector(".card-footer").innerHTML =
            "<span>" + esc(osBuild.join(" / ") || "-") + "</span>" +
            "<span>Reconn: " + (p.reconnect_count != null ? p.reconnect_count : "-") + "</span>" +
            "<span>Msg age: " + (p.last_msg_age_secs != null ? p.last_msg_age_secs + "s" : "-") + "</span>" +
            "<span>Rx: " + (p.msgs_received != null ? p.msgs_received : "-") +
            " Tx: " + (p.msgs_sent != null ? p.msgs_sent : "-") + "</span>";

        // Chart
        var history = p.history || [];
        var labels = buildTimeLabels(history, nowTs);
        var cpuData = history.map(function(s) { return s.cpu_percent; });
        var memData = history.map(function(s) { return s.memory_mb; });

        var existingChart = chartInstances.get(p.node_id);
        if (existingChart) {
            // Update existing chart
            existingChart.data.labels = labels;
            existingChart.data.datasets[0].data = cpuData;
            existingChart.data.datasets[1].data = memData;
            existingChart.update("none"); // no animation for smooth updates
        } else {
            var canvas = document.getElementById(canvasId);
            if (canvas) {
                var ctx = canvas.getContext("2d");
                var chart = new Chart(ctx, {
                    type: "line",
                    data: {
                        labels: labels,
                        datasets: [
                            {
                                label: "CPU %",
                                data: cpuData,
                                borderColor: "#42a5f5",
                                backgroundColor: "rgba(66,165,245,0.1)",
                                borderWidth: 1.5,
                                pointRadius: 0,
                                tension: 0.3,
                                yAxisID: "y",
                                fill: true
                            },
                            {
                                label: "Mem MB",
                                data: memData,
                                borderColor: "#ef5350",
                                backgroundColor: "rgba(239,83,80,0.1)",
                                borderWidth: 1.5,
                                pointRadius: 0,
                                tension: 0.3,
                                yAxisID: "y1",
                                fill: true
                            }
                        ]
                    },
                    options: {
                        responsive: true,
                        maintainAspectRatio: false,
                        animation: false,
                        interaction: {
                            mode: "index",
                            intersect: false
                        },
                        plugins: {
                            legend: {
                                display: true,
                                position: "top",
                                labels: {
                                    color: "#aaa",
                                    font: { size: 10 },
                                    boxWidth: 12,
                                    padding: 6
                                }
                            }
                        },
                        scales: {
                            x: {
                                display: true,
                                ticks: {
                                    color: "#666",
                                    font: { size: 9 },
                                    maxTicksLimit: 6
                                },
                                grid: { display: false }
                            },
                            y: {
                                type: "linear",
                                position: "left",
                                min: 0,
                                max: 100,
                                title: {
                                    display: false
                                },
                                ticks: {
                                    color: "#42a5f5",
                                    font: { size: 9 },
                                    maxTicksLimit: 4
                                },
                                grid: {
                                    color: "rgba(255,255,255,0.05)"
                                }
                            },
                            y1: {
                                type: "linear",
                                position: "right",
                                beginAtZero: true,
                                title: {
                                    display: false
                                },
                                ticks: {
                                    color: "#ef5350",
                                    font: { size: 9 },
                                    maxTicksLimit: 4
                                },
                                grid: { display: false }
                            }
                        }
                    }
                });
                chartInstances.set(p.node_id, chart);
            }
        }
    }

    // Remove cards for peers no longer in the data
    var allCards = container.querySelectorAll(".peer-card");
    for (var j = 0; j < allCards.length; j++) {
        var cid = allCards[j].id.replace("card-", "");
        if (!activePeerIds.has(cid)) {
            var oldChart = chartInstances.get(cid);
            if (oldChart) {
                oldChart.destroy();
                chartInstances.delete(cid);
            }
            allCards[j].remove();
        }
    }
}

// Persistent graph state -- survives data updates
let graphG = null;
let graphNodes = [];
let graphLinks = [];
let graphLink = null;
let graphNode = null;
let graphLabel = null;
let graphInitialized = false;

function renderGraph(data) {
    const svg = d3.select("#topology");
    const container = document.getElementById("graph-section");
    const width = container.clientWidth - 32;
    const height = 700;
    svg.attr("width", width).attr("height", height);

    // One-time initialization
    if (!graphInitialized) {
        graphG = svg.append("g");
        const zoom = d3.zoom().scaleExtent([0.2, 6]).on("zoom", (event) => {
            graphG.attr("transform", event.transform);
        });
        svg.call(zoom);
        // Set initial zoom level — 1.8x centered in the viewport
        const initialTransform = d3.zoomIdentity
            .translate(width / 2, height / 2)
            .scale(1.8)
            .translate(-width / 2, -height / 2);
        svg.call(zoom.transform, initialTransform);
        graphInitialized = true;
    }

    // Build new node/link data, preserving positions from existing nodes
    const oldPositions = {};
    for (const n of graphNodes) {
        oldPositions[n.id] = { x: n.x, y: n.y, vx: n.vx, vy: n.vy, fx: n.fx, fy: n.fy };
    }

    const nodeMap = {};
    const newNodes = data.peers.map(p => {
        const old = oldPositions[p.node_id];
        const n = {
            id: p.node_id,
            label: p.friendly_name || shortId(p.node_id),
            stale: p.stale,
        };
        // Preserve position if the node existed before
        if (old) {
            n.x = old.x; n.y = old.y;
            n.vx = old.vx; n.vy = old.vy;
            n.fx = old.fx; n.fy = old.fy;
        }
        nodeMap[p.node_id] = n;
        return n;
    });

    const newLinks = data.edges
        .filter(e => nodeMap[e.source] && nodeMap[e.target])
        .map(e => ({ source: e.source, target: e.target }));

    graphNodes = newNodes;
    graphLinks = newLinks;

    // Update edges with data join (enter/update/exit)
    graphLink = graphG.selectAll(".edge")
        .data(graphLinks, d => d.source + "-" + d.target)
        .join(
            enter => enter.append("line").attr("class", "edge"),
            update => update,
            exit => exit.remove()
        );

    // Update nodes
    graphNode = graphG.selectAll(".node")
        .data(graphNodes, d => d.id)
        .join(
            enter => enter.append("circle")
                .attr("r", 10)
                .call(d3.drag()
                    .on("start", dragStart)
                    .on("drag", dragging)
                    .on("end", dragEnd)),
            update => update,
            exit => exit.remove()
        )
        .attr("class", d => d.stale ? "node stale-node" : "node");

    // Update labels
    graphLabel = graphG.selectAll(".label")
        .data(graphNodes, d => d.id)
        .join(
            enter => enter.append("text")
                .attr("class", "label")
                .attr("dx", 14)
                .attr("dy", 4),
            update => update,
            exit => exit.remove()
        )
        .text(d => d.label);

    // Update simulation with new data -- low alpha so it doesn't bounce
    if (simulation) {
        simulation.nodes(graphNodes);
        simulation.force("link").links(graphLinks);
        simulation.alpha(0.1).restart(); // gentle nudge, not a full restart
    } else {
        simulation = d3.forceSimulation(graphNodes)
            .force("link", d3.forceLink(graphLinks).id(d => d.id).distance(80))
            .force("charge", d3.forceManyBody().strength(-200))
            .force("center", d3.forceCenter(width / 2, height / 2))
            .on("tick", tickGraph);
    }

    function tickGraph() {
        if (graphLink) graphLink
            .attr("x1", d => d.source.x).attr("y1", d => d.source.y)
            .attr("x2", d => d.target.x).attr("y2", d => d.target.y);
        if (graphNode) graphNode
            .attr("cx", d => d.x).attr("cy", d => d.y);
        if (graphLabel) graphLabel
            .attr("x", d => d.x).attr("y", d => d.y);
    }

    function dragStart(event, d) {
        if (!event.active) simulation.alphaTarget(0.3).restart();
        d.fx = d.x;
        d.fy = d.y;
    }
    function dragging(event, d) {
        d.fx = event.x;
        d.fy = event.y;
    }
    function dragEnd(event, d) {
        if (!event.active) simulation.alphaTarget(0);
        d.fx = null;
        d.fy = null;
    }
}

function render(data) {
    currentData = data;
    renderSummary(data);
    renderCards(data);
    renderTable(data);
    renderGraph(data);
}

// ---- Data fetching ----

async function fetchSwarm() {
    try {
        const resp = await fetch("/api/swarm");
        if (resp.ok) {
            const data = await resp.json();
            render(data);
        }
    } catch (e) {
        console.error("Failed to fetch swarm data:", e);
    }
}

function connectSSE() {
    const source = new EventSource("/api/events");
    source.addEventListener("swarm", (event) => {
        try {
            const data = JSON.parse(event.data);
            render(data);
        } catch (e) {
            console.error("Failed to parse SSE event:", e);
        }
    });
    source.onerror = () => {
        console.warn("SSE connection lost, will retry...");
    };
}

// ---- Suggestion Log ----

async function fetchSuggestions() {
    try {
        var resp = await fetch("/api/suggestions");
        if (resp.ok) {
            var data = await resp.json();
            renderSuggestionLog(data.entries || []);
        }
    } catch (e) {
        console.error("Failed to fetch suggestions:", e);
    }
}

function renderSuggestionLog(entries) {
    var container = document.getElementById("suggestion-log");
    if (!entries || entries.length === 0) {
        container.innerHTML = '<p class="log-empty">$ waiting for topology analysis...</p>';
        return;
    }
    // Show newest first
    var html = "";
    for (var i = entries.length - 1; i >= 0; i--) {
        var e = entries[i];
        var dt = new Date(e.timestamp * 1000);
        var h = dt.getHours().toString().padStart(2, "0");
        var m = dt.getMinutes().toString().padStart(2, "0");
        var s = dt.getSeconds().toString().padStart(2, "0");
        var mon = (dt.getMonth() + 1).toString().padStart(2, "0");
        var day = dt.getDate().toString().padStart(2, "0");
        var timeStr = dt.getFullYear() + "-" + mon + "-" + day + " " + h + ":" + m + ":" + s;
        var targetDisplay = e.target_name || shortId(e.target_node_id);
        var peerList = e.suggested_peers.map(function(p) { return shortId(p); }).join(", ");
        html += '<div class="log-entry">'
            + '<span class="log-time">[' + timeStr + ']</span> '
            + '<span class="log-reason">' + esc(e.reason) + '</span> '
            + '<span class="log-arrow">&rarr;</span> '
            + '<strong>' + esc(targetDisplay) + '</strong> '
            + '<span class="log-peers">join [' + esc(peerList) + ']</span>'
            + '</div>';
    }
    container.innerHTML = html;
}

// ---- Init ----

fetchVersion();
fetchSwarm();
fetchSuggestions();
setInterval(fetchSwarm, 10000);
setInterval(fetchSuggestions, 30000);
connectSSE();
