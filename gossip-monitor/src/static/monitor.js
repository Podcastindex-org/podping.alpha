// Podping Gossip Monitor - Frontend
"use strict";

let currentData = null;
let simulation = null;

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

function renderGraph(data) {
    const svg = d3.select("#topology");
    const container = document.getElementById("graph-section");
    const width = container.clientWidth - 32;
    const height = 400;
    svg.attr("width", width).attr("height", height);
    svg.selectAll("*").remove();

    const nodeMap = {};
    const nodes = data.peers.map(p => {
        const n = { id: p.node_id, label: p.friendly_name || shortId(p.node_id), stale: p.stale };
        nodeMap[p.node_id] = n;
        return n;
    });

    const links = data.edges
        .filter(e => nodeMap[e.source] && nodeMap[e.target])
        .map(e => ({ source: e.source, target: e.target }));

    const g = svg.append("g");

    // Zoom
    svg.call(d3.zoom().scaleExtent([0.2, 4]).on("zoom", (event) => {
        g.attr("transform", event.transform);
    }));

    const link = g.selectAll(".edge")
        .data(links)
        .join("line")
        .attr("class", "edge");

    const node = g.selectAll(".node")
        .data(nodes)
        .join("circle")
        .attr("class", d => d.stale ? "node stale-node" : "node")
        .attr("r", 10)
        .call(d3.drag()
            .on("start", dragStart)
            .on("drag", dragging)
            .on("end", dragEnd));

    const label = g.selectAll(".label")
        .data(nodes)
        .join("text")
        .attr("class", "label")
        .text(d => d.label)
        .attr("dx", 14)
        .attr("dy", 4);

    if (simulation) simulation.stop();
    simulation = d3.forceSimulation(nodes)
        .force("link", d3.forceLink(links).id(d => d.id).distance(80))
        .force("charge", d3.forceManyBody().strength(-200))
        .force("center", d3.forceCenter(width / 2, height / 2))
        .on("tick", () => {
            link.attr("x1", d => d.source.x)
                .attr("y1", d => d.source.y)
                .attr("x2", d => d.target.x)
                .attr("y2", d => d.target.y);
            node.attr("cx", d => d.x).attr("cy", d => d.y);
            label.attr("x", d => d.x).attr("y", d => d.y);
        });

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

// ---- Init ----

fetchSwarm();
setInterval(fetchSwarm, 10000);
connectSSE();
