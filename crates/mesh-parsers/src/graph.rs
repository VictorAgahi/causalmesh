use mesh_core::{ContractGraph, EdgeKind, NodeKind};
use serde::Serialize;
use std::collections::HashMap;

/// Serializable node representation for web/JSON graph export.
#[derive(Debug, Clone, Serialize)]
pub struct WebNode {
    pub id: u32,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub package: String,
    pub signature: Option<String>,
}

/// Serializable edge representation for web/JSON graph export.
#[derive(Debug, Clone, Serialize)]
pub struct WebEdge {
    pub from: u32,
    pub to: u32,
    pub kind: String,
    pub metadata: Option<String>,
}

/// Combined graph payload.
#[derive(Debug, Clone, Serialize)]
pub struct WebGraphPayload {
    pub workspace_name: String,
    pub total_nodes: usize,
    pub total_edges: usize,
    pub nodes: Vec<WebNode>,
    pub edges: Vec<WebEdge>,
}

/// Mappings and renderers for polyglot architecture mesh visualization.
pub struct GraphRenderer;

impl GraphRenderer {
    /// Extracts a serializable web payload from a ContractGraph.
    pub fn to_payload(graph: &ContractGraph, workspace_name: &str) -> WebGraphPayload {
        let nodes: Vec<WebNode> = graph
            .all_nodes()
            .map(|n| WebNode {
                id: n.id,
                name: n.name.to_string(),
                kind: format!("{:?}", n.kind),
                file_path: n.file_path.display().to_string(),
                line_start: n.line_start,
                line_end: n.line_end,
                package: n.package.to_string(),
                signature: n.signature.as_ref().map(|s| s.to_string()),
            })
            .collect();

        let edges: Vec<WebEdge> = graph
            .all_edges()
            .iter()
            .map(|e| WebEdge {
                from: e.from,
                to: e.to,
                kind: format!("{:?}", e.kind),
                metadata: e.metadata.as_ref().map(|m| m.to_string()),
            })
            .collect();

        WebGraphPayload {
            workspace_name: workspace_name.to_string(),
            total_nodes: nodes.len(),
            total_edges: edges.len(),
            nodes,
            edges,
        }
    }

    /// Exports the graph to standard JSON format.
    pub fn to_json(graph: &ContractGraph, workspace_name: &str) -> String {
        let payload = Self::to_payload(graph, workspace_name);
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    /// Exports the graph to GitHub-flavored Mermaid syntax.
    pub fn to_mermaid(graph: &ContractGraph, workspace_name: &str) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str(&format!("%% CausalMesh Topology: {workspace_name}\n"));
        out.push_str("graph TD\n");

        // Group nodes by package/service
        let mut packages: HashMap<String, Vec<&mesh_core::ContractNode>> = HashMap::new();
        for node in graph.all_nodes() {
            let pkg = if node.package.is_empty() {
                "shared".to_string()
            } else {
                node.package.to_string()
            };
            packages.entry(pkg).or_default().push(node);
        }

        let mut sorted_packages: Vec<_> = packages.into_iter().collect();
        sorted_packages.sort_by(|a, b| a.0.cmp(&b.0));

        for (pkg_name, nodes) in sorted_packages {
            let clean_subgraph_id = pkg_name.replace(['-', '.', '/', '@'], "_");
            out.push_str(&format!(
                "  subgraph sg_{clean_subgraph_id}[\"{pkg_name}\"]\n"
            ));

            for node in nodes {
                let node_id = format!("n_{}", node.id);
                let label = Self::sanitize_mermaid_label(node.name.as_str());
                let (shape_start, shape_end) = match node.kind {
                    NodeKind::KafkaTopic | NodeKind::EventStream | NodeKind::Queue => ("([", "])"),
                    NodeKind::GrpcService | NodeKind::HttpEndpoint => ("[[", "]]"),
                    NodeKind::ProtoMessage => ("{", "}"),
                    _ => ("[", "]"),
                };
                out.push_str(&format!(
                    "    {node_id}{shape_start}\"{label}\"{shape_end}\n"
                ));
            }
            out.push_str("  end\n");
        }

        out.push('\n');

        // Edges
        for edge in graph.all_edges() {
            let from_id = format!("n_{}", edge.from);
            let to_id = format!("n_{}", edge.to);

            // Only link valid destination nodes
            if graph.get_node(edge.to).is_some() {
                let arrow = match edge.kind {
                    EdgeKind::Produces => "== Produces ==> ",
                    EdgeKind::Consumes => "-. Consumes .-> ",
                    EdgeKind::CallsRpc => "-- CallsRpc --> ",
                    EdgeKind::Implements => "-- Implements --> ",
                    EdgeKind::Imports => "--> ",
                    EdgeKind::DispatchesTo => "== Dispatches ==> ",
                };
                out.push_str(&format!("  {from_id} {arrow} {to_id}\n"));
            }
        }

        out
    }

    /// Exports the graph to an autonomous, standalone interactive Dark Mode HTML5 application.
    pub fn to_html(graph: &ContractGraph, workspace_name: &str) -> String {
        let payload = Self::to_payload(graph, workspace_name);
        let json_data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

        format!(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>CausalMesh Topology &mdash; {workspace}</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600&family=Plus+Jakarta+Sans:wght@400;500;600;700&display=swap" rel="stylesheet">
  <style>
    :root {{
      --bg: #0b0c10;
      --surface: #14161f;
      --surface-border: #232738;
      --text: #e2e8f0;
      --text-muted: #8492a6;
      --accent: #38bdf8;
      --accent-purple: #a855f7;
      --accent-green: #34d399;
      --accent-amber: #fbbf24;
      --accent-rose: #fb7185;
      --font-sans: 'Plus Jakarta Sans', system-ui, -apple-system, sans-serif;
      --font-mono: 'JetBrains Mono', monospace;
    }}
    * {{ box-sizing: border-box; margin: 0; padding: 0; }}
    body {{
      background: var(--bg);
      color: var(--text);
      font-family: var(--font-sans);
      overflow: hidden;
      height: 100vh;
      display: flex;
      flex-direction: column;
    }}
    header {{
      background: rgba(20, 22, 31, 0.85);
      backdrop-filter: blur(12px);
      border-bottom: 1px solid var(--surface-border);
      padding: 12px 24px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      z-index: 10;
    }}
    .brand {{
      display: flex;
      align-items: center;
      gap: 12px;
    }}
    .brand-logo {{
      width: 32px;
      height: 32px;
      border-radius: 8px;
      background: linear-gradient(135deg, var(--accent), var(--accent-purple));
      display: flex;
      align-items: center;
      justify-content: center;
      font-weight: 700;
      color: #0b0c10;
      font-family: var(--font-mono);
      font-size: 16px;
    }}
    .brand-title {{
      font-size: 16px;
      font-weight: 700;
      letter-spacing: -0.02em;
    }}
    .brand-subtitle {{
      font-size: 12px;
      color: var(--text-muted);
      font-family: var(--font-mono);
    }}
    .stats-bar {{
      display: flex;
      align-items: center;
      gap: 16px;
    }}
    .stat-badge {{
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 10px;
      border-radius: 9999px;
      background: rgba(255, 255, 255, 0.05);
      border: 1px solid var(--surface-border);
      font-size: 12px;
      font-family: var(--font-mono);
    }}
    .stat-badge .dot {{
      width: 6px;
      height: 6px;
      border-radius: 50%;
    }}
    .controls {{
      display: flex;
      align-items: center;
      gap: 10px;
    }}
    .filter-btn-group {{
      display: inline-flex;
      background: rgba(0, 0, 0, 0.4);
      border: 1px solid var(--surface-border);
      border-radius: 6px;
      padding: 2px;
    }}
    .filter-btn {{
      background: transparent;
      border: none;
      color: var(--text-muted);
      font-family: var(--font-sans);
      font-size: 12px;
      font-weight: 600;
      padding: 4px 10px;
      border-radius: 4px;
      cursor: pointer;
      transition: all 0.2s;
    }}
    .filter-btn.active {{
      background: var(--accent);
      color: #0b0c10;
    }}
    .search-input {{
      background: var(--surface);
      border: 1px solid var(--surface-border);
      color: var(--text);
      font-family: var(--font-mono);
      font-size: 13px;
      padding: 6px 12px;
      border-radius: 6px;
      outline: none;
      width: 200px;
      transition: border-color 0.2s;
    }}
    .search-input:focus {{
      border-color: var(--accent);
    }}
    .btn {{
      background: rgba(255, 255, 255, 0.08);
      border: 1px solid var(--surface-border);
      color: var(--text);
      font-family: var(--font-sans);
      font-size: 13px;
      font-weight: 500;
      padding: 6px 12px;
      border-radius: 6px;
      cursor: pointer;
      display: inline-flex;
      align-items: center;
      gap: 6px;
      transition: background 0.2s, border-color 0.2s;
    }}
    .btn:hover {{
      background: rgba(255, 255, 255, 0.14);
      border-color: var(--accent);
    }}
    #app-container {{
      position: relative;
      flex: 1;
      width: 100%;
      overflow: hidden;
    }}
    canvas {{
      width: 100%;
      height: 100%;
      display: block;
      cursor: grab;
    }}
    canvas:active {{
      cursor: grabbing;
    }}
    .sidebar {{
      position: absolute;
      top: 16px;
      right: 16px;
      width: 340px;
      max-height: calc(100% - 32px);
      background: rgba(20, 22, 31, 0.95);
      backdrop-filter: blur(16px);
      border: 1px solid var(--surface-border);
      border-radius: 12px;
      padding: 20px;
      overflow-y: auto;
      display: none;
      box-shadow: 0 20px 40px rgba(0, 0, 0, 0.5);
      z-index: 5;
    }}
    .sidebar.active {{
      display: block;
      animation: slideIn 0.2s ease-out;
    }}
    @keyframes slideIn {{
      from {{ transform: translateX(20px); opacity: 0; }}
      to {{ transform: translateX(0); opacity: 1; }}
    }}
    .sidebar-header {{
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      margin-bottom: 12px;
    }}
    .node-type-tag {{
      font-family: var(--font-mono);
      font-size: 11px;
      padding: 2px 8px;
      border-radius: 4px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: 0.05em;
    }}
    .node-title {{
      font-size: 16px;
      font-weight: 700;
      color: #fff;
      word-break: break-all;
      margin-top: 6px;
    }}
    .property-group {{
      margin-top: 14px;
    }}
    .property-label {{
      font-size: 11px;
      text-transform: uppercase;
      letter-spacing: 0.05em;
      color: var(--text-muted);
      font-weight: 600;
      margin-bottom: 4px;
    }}
    .property-value {{
      font-family: var(--font-mono);
      font-size: 12px;
      background: rgba(0, 0, 0, 0.3);
      padding: 6px 10px;
      border-radius: 6px;
      border: 1px solid rgba(255, 255, 255, 0.05);
      color: var(--accent);
      word-break: break-all;
    }}
    .legend {{
      position: absolute;
      bottom: 16px;
      left: 16px;
      background: rgba(20, 22, 31, 0.85);
      backdrop-filter: blur(12px);
      border: 1px solid var(--surface-border);
      border-radius: 8px;
      padding: 10px 14px;
      display: flex;
      gap: 16px;
      font-size: 11px;
      font-family: var(--font-mono);
      z-index: 5;
    }}
    .legend-item {{
      display: flex;
      align-items: center;
      gap: 6px;
    }}
    .legend-color {{
      width: 10px;
      height: 10px;
      border-radius: 2px;
    }}
    .toast {{
      position: absolute;
      bottom: 24px;
      right: 24px;
      background: var(--accent);
      color: #0b0c10;
      font-weight: 600;
      font-size: 13px;
      padding: 8px 16px;
      border-radius: 6px;
      opacity: 0;
      pointer-events: none;
      transition: opacity 0.3s;
      z-index: 100;
    }}
    .toast.show {{
      opacity: 1;
    }}
  </style>
</head>
<body>
  <header>
    <div class="brand">
      <div class="brand-logo">CM</div>
      <div>
        <div class="brand-title">CausalMesh Interactive Topology</div>
        <div class="brand-subtitle">{workspace} &bull; Polyglot Contract Graph</div>
      </div>
    </div>
    <div class="stats-bar">
      <div class="stat-badge">
        <span class="dot" style="background: var(--accent);"></span>
        <span>Visible Nodes: <strong id="stat-nodes">0</strong></span>
      </div>
      <div class="stat-badge">
        <span class="dot" style="background: var(--accent-purple);"></span>
        <span>Edges: <strong id="stat-edges">0</strong></span>
      </div>
    </div>
    <div class="controls">
      <div class="filter-btn-group">
        <button class="filter-btn active" id="filter-contracts">Contracts & Flows</button>
        <button class="filter-btn" id="filter-all">All Symbols</button>
      </div>
      <input type="text" id="search-input" class="search-input" placeholder="Search symbol or service..." />
      <button class="btn" id="btn-reset">Reset</button>
      <button class="btn" id="btn-copy-mermaid">Copy Mermaid</button>
    </div>
  </header>

  <div id="app-container">
    <canvas id="viewport"></canvas>

    <div id="sidebar" class="sidebar">
      <div class="sidebar-header">
        <span id="node-tag" class="node-type-tag"></span>
        <button id="btn-close-sidebar" class="btn" style="padding: 2px 6px;">&times;</button>
      </div>
      <div id="node-title" class="node-title"></div>

      <div class="property-group">
        <div class="property-label">Package / Service</div>
        <div id="node-package" class="property-value"></div>
      </div>

      <div class="property-group">
        <div class="property-label">Source Location</div>
        <div id="node-location" class="property-value"></div>
      </div>

      <div id="signature-group" class="property-group" style="display: none;">
        <div class="property-label">Signature / Schema</div>
        <pre id="node-signature" class="property-value" style="white-space: pre-wrap; font-size: 11px;"></pre>
      </div>
    </div>

    <div class="legend">
      <div class="legend-item"><div class="legend-color" style="background: #38bdf8;"></div>gRPC Service / Method</div>
      <div class="legend-item"><div class="legend-color" style="background: #fbbf24;"></div>Kafka / Queue Topic</div>
      <div class="legend-item"><div class="legend-color" style="background: #34d399;"></div>Protobuf Message</div>
      <div class="legend-item"><div class="legend-color" style="background: #a855f7;"></div>HTTP Endpoint</div>
      <div class="legend-item"><div class="legend-color" style="background: #fb7185;"></div>Saga / Outbox</div>
    </div>

    <div id="toast" class="toast">Mermaid Copied to Clipboard!</div>
  </div>

  <script id="graph-data" type="application/json">
{data}
  </script>

  <script>
    const rawData = JSON.parse(document.getElementById('graph-data').textContent);

    const canvas = document.getElementById('viewport');
    const ctx = canvas.getContext('2d');
    const sidebar = document.getElementById('sidebar');

    function resizeCanvas() {{
      canvas.width = canvas.parentElement.clientWidth * window.devicePixelRatio;
      canvas.height = canvas.parentElement.clientHeight * window.devicePixelRatio;
      render();
    }}
    window.addEventListener('resize', resizeCanvas);

    function getNodeColor(kind) {{
      switch (kind) {{
        case 'GrpcService':
        case 'GrpcMethod': return '#38bdf8';
        case 'KafkaTopic':
        case 'EventStream':
        case 'Queue': return '#fbbf24';
        case 'ProtoMessage': return '#34d399';
        case 'HttpEndpoint': return '#a855f7';
        case 'Saga':
        case 'PostProcessor': return '#fb7185';
        default: return '#64748b';
      }}
    }}

    function isContractNode(n) {{
      return n.kind !== 'ServiceClass' &&
             n.kind !== 'Interface' &&
             n.kind !== 'Other' ||
             rawData.edges.some(e => e.from === n.id || e.to === n.id);
    }}

    let showOnlyContracts = true;
    let nodes = [];
    let edges = [];
    let clusters = new Map();
    let nodeMap = new Map();

    function buildLayout() {{
      const candidateNodes = showOnlyContracts
        ? rawData.nodes.filter(isContractNode)
        : rawData.nodes;

      const activeIds = new Set(candidateNodes.map(n => n.id));
      const activeEdges = rawData.edges.filter(e => activeIds.has(e.from) && activeIds.has(e.to));

      // Group candidate nodes by package
      const pkgMap = new Map();
      candidateNodes.forEach(n => {{
        const pkg = n.package || 'shared';
        if (!pkgMap.has(pkg)) pkgMap.set(pkg, []);
        pkgMap.get(pkg).push(n);
      }});

      clusters.clear();
      nodeMap.clear();
      nodes = [];

      const pkgCount = pkgMap.size;
      const cols = Math.ceil(Math.sqrt(pkgCount));
      const clusterSpacingX = 360;
      const clusterSpacingY = 320;

      let pkgIdx = 0;
      pkgMap.forEach((pkgNodes, pkgName) => {{
        const row = Math.floor(pkgIdx / cols);
        const col = pkgIdx % cols;
        const cx = (col - (cols - 1) / 2) * clusterSpacingX;
        const cy = (row - Math.floor(pkgCount / cols) / 2) * clusterSpacingY;

        const clusterRadius = Math.max(90, Math.min(180, Math.sqrt(pkgNodes.length) * 35));
        clusters.set(pkgName, {{
          name: pkgName,
          x: cx,
          y: cy,
          radius: clusterRadius,
          count: pkgNodes.length
        }});

        pkgNodes.forEach((n, i) => {{
          const subAngle = (i / pkgNodes.length) * Math.PI * 2;
          const dist = pkgNodes.length === 1 ? 0 : Math.min(clusterRadius - 25, 30 + (i % 3) * 30);
          const nodeObj = {{
            ...n,
            x: cx + Math.cos(subAngle) * dist,
            y: cy + Math.sin(subAngle) * dist,
            vx: 0,
            vy: 0,
            radius: n.kind === 'GrpcService' || n.kind === 'KafkaTopic' ? 14 : 10,
            color: getNodeColor(n.kind)
          }};
          nodes.push(nodeObj);
          nodeMap.set(nodeObj.id, nodeObj);
        }});

        pkgIdx++;
      }});

      edges = activeEdges.filter(e => nodeMap.has(e.from) && nodeMap.has(e.to)).map(e => ({{
        ...e,
        source: nodeMap.get(e.from),
        target: nodeMap.get(e.to)
      }}));

      document.getElementById('stat-nodes').textContent = nodes.length;
      document.getElementById('stat-edges').textContent = edges.length;
      render();
    }}

    // View transform
    let scale = 0.9;
    let panX = 0;
    let panY = 0;
    let isDragging = false;
    let draggedNode = null;
    let lastMouseX = 0;
    let lastMouseY = 0;
    let hoveredNode = null;
    let selectedNode = null;
    let searchQuery = '';

    function resetView() {{
      scale = 0.9;
      panX = (canvas.width / (window.devicePixelRatio || 1)) / 2;
      panY = (canvas.height / (window.devicePixelRatio || 1)) / 2;
      render();
    }}

    canvas.addEventListener('wheel', e => {{
      e.preventDefault();
      const zoomFactor = e.deltaY < 0 ? 1.12 : 0.88;
      scale = Math.min(Math.max(0.15, scale * zoomFactor), 4);
      render();
    }});

    canvas.addEventListener('mousedown', e => {{
      const rect = canvas.getBoundingClientRect();
      const mouseX = (e.clientX - rect.left - panX) / scale;
      const mouseY = (e.clientY - rect.top - panY) / scale;

      const hit = nodes.find(n => Math.hypot(n.x - mouseX, n.y - mouseY) <= n.radius * 1.5);
      if (hit) {{
        draggedNode = hit;
        selectedNode = hit;
        openSidebar(hit);
        render();
        return;
      }}

      isDragging = true;
      lastMouseX = e.clientX;
      lastMouseY = e.clientY;
    }});

    window.addEventListener('mousemove', e => {{
      const rect = canvas.getBoundingClientRect();
      const mouseX = (e.clientX - rect.left - panX) / scale;
      const mouseY = (e.clientY - rect.top - panY) / scale;

      if (draggedNode) {{
        draggedNode.x = mouseX;
        draggedNode.y = mouseY;
        render();
        return;
      }}

      if (isDragging) {{
        panX += e.clientX - lastMouseX;
        panY += e.clientY - lastMouseY;
        lastMouseX = e.clientX;
        lastMouseY = e.clientY;
        render();
        return;
      }}

      const hit = nodes.find(n => Math.hypot(n.x - mouseX, n.y - mouseY) <= n.radius * 1.5);
      if (hit !== hoveredNode) {{
        hoveredNode = hit;
        canvas.style.cursor = hit ? 'pointer' : 'grab';
        render();
      }}
    }});

    window.addEventListener('mouseup', () => {{
      isDragging = false;
      draggedNode = null;
    }});

    function openSidebar(node) {{
      sidebar.classList.add('active');
      const tag = document.getElementById('node-tag');
      tag.textContent = node.kind;
      tag.style.background = node.color + '22';
      tag.style.color = node.color;
      document.getElementById('node-title').textContent = node.name;
      document.getElementById('node-package').textContent = node.package || 'shared';
      document.getElementById('node-location').textContent = `${{node.file_path}}:${{node.line_start}}-${{node.line_end}}`;

      const sigGroup = document.getElementById('signature-group');
      if (node.signature) {{
        sigGroup.style.display = 'block';
        document.getElementById('node-signature').textContent = node.signature;
      }} else {{
        sigGroup.style.display = 'none';
      }}
    }}

    document.getElementById('btn-close-sidebar').addEventListener('click', () => {{
      sidebar.classList.remove('active');
      selectedNode = null;
      render();
    }});

    document.getElementById('btn-reset').addEventListener('click', resetView);

    document.getElementById('filter-contracts').addEventListener('click', () => {{
      showOnlyContracts = true;
      document.getElementById('filter-contracts').classList.add('active');
      document.getElementById('filter-all').classList.remove('active');
      buildLayout();
      resetView();
    }});

    document.getElementById('filter-all').addEventListener('click', () => {{
      showOnlyContracts = false;
      document.getElementById('filter-all').classList.add('active');
      document.getElementById('filter-contracts').classList.remove('active');
      buildLayout();
      resetView();
    }});

    document.getElementById('search-input').addEventListener('input', e => {{
      searchQuery = e.target.value.toLowerCase().trim();
      render();
    }});

    function render() {{
      const dpr = window.devicePixelRatio || 1;
      ctx.save();
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      ctx.scale(dpr, dpr);
      ctx.translate(panX, panY);
      ctx.scale(scale, scale);

      // 1. Draw Cluster Background Cards
      clusters.forEach(c => {{
        ctx.beginPath();
        ctx.arc(c.x, c.y, c.radius + 18, 0, Math.PI * 2);
        ctx.fillStyle = 'rgba(20, 22, 31, 0.45)';
        ctx.fill();
        ctx.strokeStyle = 'rgba(255, 255, 255, 0.05)';
        ctx.lineWidth = 1;
        ctx.stroke();

        // Cluster Title Pill
        ctx.font = '600 12px "JetBrains Mono", monospace';
        const titleText = `📦 ${{c.name}} (${{c.count}})`;
        const textWidth = ctx.measureText(titleText).width;

        ctx.fillStyle = 'rgba(11, 12, 16, 0.85)';
        ctx.beginPath();
        ctx.roundRect(c.x - textWidth / 2 - 8, c.y - c.radius - 32, textWidth + 16, 22, 4);
        ctx.fill();
        ctx.strokeStyle = 'rgba(255, 255, 255, 0.15)';
        ctx.stroke();

        ctx.fillStyle = '#cbd5e1';
        ctx.textAlign = 'center';
        ctx.fillText(titleText, c.x, c.y - c.radius - 17);
      }});

      // 2. Draw Curved Edges
      edges.forEach(e => {{
        const isConnected = selectedNode && (selectedNode.id === e.from || selectedNode.id === e.to);
        const isDimmed = selectedNode && !isConnected;

        ctx.beginPath();
        const midX = (e.source.x + e.target.x) / 2;
        const midY = (e.source.y + e.target.y) / 2;
        const dx = e.target.x - e.source.x;
        const dy = e.target.y - e.source.y;
        const normalX = -dy * 0.15;
        const normalY = dx * 0.15;

        ctx.moveTo(e.source.x, e.source.y);
        ctx.quadraticCurveTo(midX + normalX, midY + normalY, e.target.x, e.target.y);

        ctx.strokeStyle = isConnected
          ? '#38bdf8'
          : isDimmed
            ? 'rgba(255, 255, 255, 0.03)'
            : 'rgba(56, 189, 248, 0.25)';
        ctx.lineWidth = isConnected ? 2.5 : 1.2;
        ctx.stroke();
      }});

      // 3. Draw Nodes with Smart Level of Detail
      nodes.forEach(n => {{
        const isMatched = !searchQuery || n.name.toLowerCase().includes(searchQuery) || n.package.toLowerCase().includes(searchQuery);
        const isSelected = selectedNode && selectedNode.id === n.id;
        const isHovered = hoveredNode && hoveredNode.id === n.id;
        const isDimmed = selectedNode && !isSelected && !edges.some(e => (e.from === selectedNode.id && e.to === n.id) || (e.to === selectedNode.id && e.from === n.id));

        // Node Glow & Circle
        ctx.beginPath();
        ctx.arc(n.x, n.y, n.radius * (isSelected || isHovered ? 1.35 : 1), 0, Math.PI * 2);

        if (isDimmed) {{
          ctx.fillStyle = 'rgba(51, 65, 85, 0.25)';
          ctx.fill();
        }} else {{
          ctx.fillStyle = isMatched ? n.color : '#334155';
          if (isSelected || isHovered || isMatched && searchQuery) {{
            ctx.shadowColor = n.color;
            ctx.shadowBlur = 16;
          }}
          ctx.fill();
          ctx.shadowBlur = 0;
        }}

        // Level of Detail for labels:
        // Only draw labels if:
        // a) Zoomed in (scale > 1.1)
        // b) Node is hovered or selected
        // c) Search matches
        // d) Contracts-only mode with low density
        const shouldShowLabel = (scale >= 1.0) || isSelected || isHovered || (searchQuery && isMatched) || (nodes.length <= 40);

        if (shouldShowLabel && !isDimmed) {{
          ctx.font = '500 11px "JetBrains Mono", monospace';
          const labelWidth = ctx.measureText(n.name).width;

          // Draw pill background to prevent text collision
          ctx.fillStyle = 'rgba(11, 12, 16, 0.9)';
          ctx.beginPath();
          ctx.roundRect(n.x - labelWidth / 2 - 4, n.y + n.radius + 4, labelWidth + 8, 16, 3);
          ctx.fill();
          ctx.strokeStyle = isSelected || isHovered ? n.color : 'rgba(255, 255, 255, 0.1)';
          ctx.lineWidth = 1;
          ctx.stroke();

          ctx.fillStyle = isSelected || isHovered ? '#38bdf8' : '#f1f5f9';
          ctx.textAlign = 'center';
          ctx.fillText(n.name, n.x, n.y + n.radius + 16);
        }}
      }});

      ctx.restore();
    }}

    // Mermaid copy button
    document.getElementById('btn-copy-mermaid').addEventListener('click', () => {{
      let mermaid = 'graph TD\n';
      nodes.forEach(n => {{
        mermaid += `  n_${{n.id}}["${{n.name}}"]\n`;
      }});
      edges.forEach(e => {{
        mermaid += `  n_${{e.from}} -->|${{e.kind}}| n_${{e.to}}\n`;
      }});
      navigator.clipboard.writeText(mermaid).then(() => {{
        const toast = document.getElementById('toast');
        toast.classList.add('show');
        setTimeout(() => toast.classList.remove('show'), 2000);
      }});
    }});

    // Initialize layout
    resizeCanvas();
    buildLayout();
    resetView();
  </script>
</body>
</html>"#,
            workspace = workspace_name,
            data = json_data
        )
    }

    fn sanitize_mermaid_label(s: &str) -> String {
        s.replace(['"', ';', '<', '>', '{', '}', '[', ']'], "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{ContractEdge, ContractNode, EdgeKind, NodeKind};
    use std::path::PathBuf;

    #[test]
    fn test_graph_export_mermaid_and_html() {
        let mut graph = ContractGraph::new();
        let id1 = graph.add_node(ContractNode {
            id: 0,
            name: "OrderCreatedEvent".into(),
            kind: NodeKind::KafkaTopic,
            file_path: PathBuf::from("proto/events.proto"),
            line_start: 10,
            line_end: 20,
            package: "events".into(),
            repo_id: 1,
            signature: Some("message OrderCreatedEvent { string id = 1; }".into()),
            docstring: None,
        });

        let id2 = graph.add_node(ContractNode {
            id: 0,
            name: "PaymentProcessor".into(),
            kind: NodeKind::GrpcService,
            file_path: PathBuf::from("services/payment/Payment.ts"),
            line_start: 30,
            line_end: 50,
            package: "payment-service".into(),
            repo_id: 1,
            signature: None,
            docstring: None,
        });

        graph.add_edge(ContractEdge {
            from: id2,
            to: id1,
            kind: EdgeKind::Produces,
            metadata: Some("orders.created".into()),
        });

        let mermaid = GraphRenderer::to_mermaid(&graph, "test-workspace");
        assert!(mermaid.contains("OrderCreatedEvent"));
        assert!(mermaid.contains("PaymentProcessor"));
        assert!(mermaid.contains("Produces"));

        let html = GraphRenderer::to_html(&graph, "test-workspace");
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("CausalMesh Interactive Topology"));
        assert!(html.contains("OrderCreatedEvent"));
    }
}
