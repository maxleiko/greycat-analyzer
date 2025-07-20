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
  } else {
    li.textContent = node.name || node.type || 'Unknown';
  }

  if (node.span) {
    const span = document.createElement('span');
    span.className = 'span';
    span.textContent = `[${node.span.start.offset}:${node.span.end.offset}]`;
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
  const source = mEditor.getValue();
  localStorage.setItem('source-code', source);
  const root = parse_cst(source);
  rootUL.replaceChildren(renderNode(root));
}

function editor_update() {
  update_tree();
}

function span(node) {
  switch (node.type) {
    case 'Node': {
      const start = span(node.children[0]).start;
      const end = span(node.children[node.children.length - 1]).end;
      return { start, end };
    }
    case 'Token': {
      return node.span;
    }
    case 'Error': {
      return node.token.span;
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
});

mEditor.onDidChangeModelContent(editor_update);

editor_update();
