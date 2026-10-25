// Draws the Mermaid diagrams on a spec page. The library is one vendored file, built by
// scripts/build_web_mermaid.sh. Its name changes whenever its bytes do; keep this import and
// MERMAID_JS_NAME in src/web_app/pages.rs the same.
import mermaid from "/static/mermaid-12.1.0-9918635a.js";

const dark = matchMedia("(prefers-color-scheme: dark)");
const diagrams = [...document.querySelectorAll("pre.mermaid")].map((node) => ({
  node,
  source: node.textContent,
}));

function start() {
  // 'strict' keeps a diagram free of click handlers and HTML labels.
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    layout: "dagre",
    theme: dark.matches ? "dark" : "neutral",
  });
}

async function draw() {
  start();
  for (const [index, diagram] of diagrams.entries()) {
    try {
      const { svg } = await mermaid.render(`spec-diagram-${index}`, diagram.source);
      diagram.node.classList.remove("failed");
      diagram.node.innerHTML = svg;
    } catch (_error) {
      // A diagram that fails to draw shows its source.
      document.getElementById(`dspec-diagram-${index}`)?.remove();
      diagram.node.classList.add("failed");
      diagram.node.textContent = diagram.source;
    }
  }
}

if (diagrams.length > 0) {
  draw();
  dark.addEventListener("change", draw);
}
