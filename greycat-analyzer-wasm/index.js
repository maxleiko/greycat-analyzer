// @ts-check
import init, { parse_cst } from './pkg/greycat_analyzer_wasm.js';

await init();

function renderNode(node) {
  const li = document.createElement('li');

  const hasChildren = Array.isArray(node.children) && node.children.length > 0;

  if (hasChildren) {
    const toggle = document.createElement('span');
    toggle.textContent = '▾ ' + (node.name || node.type);
    toggle.className = 'toggle';
    toggle.onclick = () => {
      li.classList.toggle('collapsed');
      toggle.textContent =
        (li.classList.contains('collapsed') ? '▸ ' : '▾ ') +
        (node.name || node.type);
    };
    li.appendChild(toggle);
  } else if (node.type === 'Token') {
    const text = `${node.kind.kind}${
      node.kind.value !== undefined ? `(${node.kind.value})` : ''
    }`;
    li.innerHTML = `<span class="token">${text}</span>`;
  } else {
    li.textContent = node.name || node.type || 'Unknown';
  }

  if (node.span) {
    const span = document.createElement('span');
    span.className = 'span';
    span.textContent = `(${node.span.start.join(',')} → ${node.span.end.join(
      ','
    )})`;
    li.appendChild(span);
  }

  if (hasChildren) {
    const ul = document.createElement('ul');
    node.children.forEach((child) => {
      ul.appendChild(renderNode(child));
    });
    li.appendChild(ul);
  }

  return li;
}

function update_tree() {
  const source = editor.textContent;
  if (source) {
    localStorage.setItem('source-code', source);
    const root = parse_cst(source);
    rootUL.replaceChildren(renderNode(root));
  }
}

const rootUL = document.createElement('ul');
tree.appendChild(rootUL);
const prev_source = localStorage.getItem('source-code');
if (prev_source) {
  editor.textContent = prev_source;
}
function updateLineNumbers() {
  const lines = editor.innerText.split('\n').length;
  lineNumbers.textContent = Array.from({ length: lines }, (_, i) => i + 1).join(
    '\n'
  );
}
function editor_update() {
  updateLineNumbers();
  update_tree();
}

editor.addEventListener('input', editor_update);
editor.addEventListener('paste', (e) => {
  // Prevent the default paste behavior
  e.preventDefault();

  // Get plain text
  const text = e.clipboardData?.getData('text/plain');
  if (!text) return;

  // Get current selection
  const selection = window.getSelection();
  if (!selection || !selection.rangeCount) return;

  // Replace the current range with plain text
  selection.deleteFromDocument();
  selection.getRangeAt(0).insertNode(document.createTextNode(text));

  // Move cursor to the end of the inserted text
  selection.collapseToEnd();
  editor.dispatchEvent(new InputEvent('input', { bubbles: true }));
});
editor.addEventListener('scroll', () => {
  lineNumbers.scrollTop = editor.scrollTop;
});

editor_update();
