/// <reference path="./global.d.ts" />

// @ts-check
import init, { parse_cst } from './pkg/greycat_analyzer_wasm.js';

await init();

function renderNode(node) {
  const li = document.createElement('li');

  // Make item selectable
  li.onclick = (e) => {
    // Remove 'selected' from previous item
    const prev = tree.querySelector('.selected');
    if (prev) {
      prev.classList.remove('selected');
    }

    // Add 'selected' to this item
    li.classList.add('selected');
    e.stopPropagation();
    tree.dispatchEvent(new CustomEvent('tree-select', { detail: node }));
  };

  const hasChildren = Array.isArray(node.children) && node.children.length > 0;

  if (hasChildren) {
    const toggle = document.createElement('span');
    toggle.textContent = '- ' + (node.name || node.type);
    toggle.className = 'toggle';
    toggle.onclick = () => {
      li.classList.toggle('collapsed');
      toggle.textContent =
        (li.classList.contains('collapsed') ? '+ ' : '- ') +
        (node.name || node.type);
    };
    li.appendChild(toggle);
  } else if (node.type === 'Token') {
    const text = `${node.kind.kind}${
      node.kind.value !== undefined ? `(${node.kind.value})` : ''
    }`;
    li.innerHTML = `<span class="token">${text}</span>`;
  } else if (node.type === 'Error') {
    console.log(node);
    li.innerHTML = `<span class="error">${node.kind.reason} ${node.kind.expected}</span><span class="error-kind">(${node.kind.got.kind})</span>`;
  } else {
    li.innerHTML = `<span class="toggle">${node.name}</span>`;
  }

  const s = span(node);
  const spanEl = document.createElement('span');
  spanEl.className = 'span';
  spanEl.textContent = `[${s.start.offset}:${s.end.offset}]`;
  li.appendChild(spanEl);

  if (hasChildren) {
    const ul = document.createElement('ul');
    node.children.forEach((child) => {
      // if (child.type === 'Token' && child.kind.kind === 'Space') {
      //   return;
      // }
      ul.appendChild(renderNode(child));
    });
    li.appendChild(ul);
  }

  return li;
}

function update_tree() {
  const source = mEditor.getValue();
  localStorage.setItem('source-code', source);
  const start = performance.now();
  const root = parse_cst(source);
  const elapsed = performance.now() - start;
  console.log('parse', elapsed, root);
  rootUL.replaceChildren(renderNode(root));
}

function editor_update() {
  update_tree();
}

function span(node) {
  switch (node.type) {
    default:
    case 'Node': {
      if (node.children.length > 0) {
        const start = span(node.children[0]).start;
        const end = span(node.children[node.children.length - 1]).end;
        return { start, end };
      }
      return {
        start: { line: 0, column: 0, offset: 0 },
        end: { line: 0, column: 0, offset: 0 },
      };
    }
    case 'Token': {
      return node.span;
    }
    case 'Error': {
      return node.span;
    }
  }
}

const rootUL = document.createElement('ul');

tree.appendChild(rootUL);
tree.addEventListener('tree-select', (e) => {
  const node = e.detail;
  console.log('select', node);
  const s = span(node);
  const selection = {
    startLineNumber: s.start.line + 1,
    startColumn: s.start.column + 1,
    endLineNumber: s.end.line + 1,
    endColumn: s.end.column + 1,
  };
  mEditor.setSelection(selection);
  mEditor.revealRangeInCenter(selection, monaco.editor.ScrollType.Immediate);
});
const prev_source = localStorage.getItem('source-code');
// Create editor instance
const mEditor = monaco.editor.create(editor, {
  value: prev_source ?? '',
  language: 'greycat',
  theme: 'vs-dark',
  minimap: { enabled: false },
});

mEditor.onDidChangeModelContent(editor_update);

editor_update();
