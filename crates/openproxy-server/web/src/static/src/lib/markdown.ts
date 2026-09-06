// lib/markdown.ts — Markdown + LaTeX renderer — lib pura sin dependencias de UI.
//
// Self-contained pipeline: escapes HTML, extracts fenced code blocks and
// inline math placeholders, parses Markdown structural blocks (tables,
// blockquotes, lists, headings, HR), and substitutes LaTeX fragments with
// HTML spans. Produces a string of HTML that the caller may render via
// `lit-html/directives/unsafe-html.js`.
//
// The only runtime side effect is the rendered string's `<button>` elements
// invoke `window.__copyPlaygroundCode(btn)` for fenced code copy; the
// playground installs that handler in `views/playground/shared.ts`. This
// module itself does not import anything from `views/` or `state/`.

const LATEX_SYMBOL_PAIRS: [string, string][] = [
  ['\\longleftrightarrow', '⟷'],
  ['\\Longleftrightarrow', '⟺'],
  ['\\longrightarrow', '⟶'],
  ['\\Longrightarrow', '⟹'],
  ['\\leftrightarrow', '↔'],
  ['\\Leftrightarrow', '⇔'],
  ['\\rightarrow', '→'],
  ['\\Rightarrow', '⇒'],
  ['\\leftarrow', '←'],
  ['\\Leftarrow', '⇐'],
  ['\\subseteq', '⊆'],
  ['\\supseteq', '⊇'],
  ['\\setminus', '∖'],
  ['\\emptyset', '∅'],
  ['\\varnothing', '∅'],
  ['\\nexists', '∄'],
  ['\\approx', '≈'],
  ['\\equiv', '≡'],
  ['\\propto', '∝'],
  ['\\notin', '∉'],
  ['\\subset', '⊂'],
  ['\\supset', '⊃'],
  ['\\forall', '∀'],
  ['\\exists', '∃'],
  ['\\partial', '∂'],
  ['\\nabla', '∇'],
  ['\\infty', '∞'],
  ['\\times', '×'],
  ['\\cdot', '·'],
  ['\\div', '÷'],
  ['\\pm', '±'],
  ['\\mp', '∓'],
  ['\\le', '≤'],
  ['\\leq', '≤'],
  ['\\ge', '≥'],
  ['\\geq', '≥'],
  ['\\ne', '≠'],
  ['\\neq', '≠'],
  ['\\ll', '≪'],
  ['\\gg', '≫'],
  ['\\in', '∈'],
  ['\\cap', '∩'],
  ['\\cup', '∪'],
  ['\\sum', '<span class="math-op">∑</span>'],
  ['\\prod', '<span class="math-op">∏</span>'],
  ['\\iint', '<span class="math-op">∬</span>'],
  ['\\iiint', '<span class="math-op">∭</span>'],
  ['\\oint', '<span class="math-op">∮</span>'],
  ['\\int', '<span class="math-op">∫</span>'],
  ['\\to', '→'],
  ['\\implies', '⇒'],
  ['\\iff', '⇔'],
  ['\\mapsto', '↦'],
  ['\\ldots', '…'],
  ['\\cdots', '⋯'],
  ['\\ddots', '⋱'],
  ['\\vdots', '⋮'],
  ['\\dots', '…'],
  ['\\alpha', 'α'],
  ['\\beta', 'β'],
  ['\\gamma', 'γ'],
  ['\\delta', 'δ'],
  ['\\epsilon', 'ε'],
  ['\\varepsilon', 'ε'],
  ['\\zeta', 'ζ'],
  ['\\eta', 'η'],
  ['\\theta', 'θ'],
  ['\\vartheta', 'ϑ'],
  ['\\iota', 'ι'],
  ['\\kappa', 'κ'],
  ['\\lambda', 'λ'],
  ['\\mu', 'μ'],
  ['\\nu', 'ν'],
  ['\\xi', 'ξ'],
  ['\\pi', 'π'],
  ['\\varpi', 'ϖ'],
  ['\\rho', 'ρ'],
  ['\\varrho', 'ϱ'],
  ['\\sigma', 'σ'],
  ['\\varsigma', 'ς'],
  ['\\tau', 'τ'],
  ['\\upsilon', 'υ'],
  ['\\phi', 'φ'],
  ['\\varphi', 'ϕ'],
  ['\\chi', 'χ'],
  ['\\psi', 'ψ'],
  ['\\omega', 'ω'],
  ['\\Gamma', 'Γ'],
  ['\\Delta', 'Δ'],
  ['\\Theta', 'Θ'],
  ['\\Lambda', 'Λ'],
  ['\\Xi', 'Ξ'],
  ['\\Pi', 'Π'],
  ['\\Sigma', 'Σ'],
  ['\\Upsilon', 'Υ'],
  ['\\Phi', 'Φ'],
  ['\\Psi', 'Ψ'],
  ['\\Omega', 'Ω'],
  ['\\deg', '°'],
  ['\\circ', '°'],
  ['\\angle', '∠'],
  ['\\perp', '⊥'],
  ['\\mid', '|'],
  ['\\parallel', '∥'],
  ['\\sim', '∼'],
  ['\\ast', '∗'],
  ['\\star', '⋆'],
];

function escapeHtml(str: string): string {
  return str
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function parseFractions(str: string): string {
  let changed = true;
  let iterations = 0;
  while (changed && iterations < 8) {
    iterations++;
    const next = str.replace(/\\(?:d?frac)\{([^{}]+)\}\{([^{}]+)\}/g, (_m, num, den) => {
      return `<span class="math-frac"><span class="math-num">${num}</span><span class="math-den">${den}</span></span>`;
    });
    changed = next !== str;
    str = next;
  }
  return str;
}

function parseSqrt(str: string): string {
  let changed = true;
  let iterations = 0;
  while (changed && iterations < 8) {
    iterations++;
    let next = str.replace(/\\sqrt\[([^{}\]]+)\]\{([^{}]+)\}/g, (_m, deg, rad) => {
      return `<span class="math-sqrt"><sup class="math-sqrt-deg">${deg}</sup><span class="math-sqrt-sign">√</span><span class="math-sqrt-radicand">${rad}</span></span>`;
    });
    next = next.replace(/\\sqrt\{([^{}]+)\}/g, (_m, rad) => {
      return `<span class="math-sqrt"><span class="math-sqrt-sign">√</span><span class="math-sqrt-radicand">${rad}</span></span>`;
    });
    changed = next !== str;
    str = next;
  }
  return str;
}

function parseScripts(str: string): string {
  // Superscripts with braces and single character
  str = str.replace(/\^{([^{}]+)}/g, '<sup>$1</sup>');
  str = str.replace(/\^([a-zA-Z0-9+\-α-ωΑ-Ω])/g, '<sup>$1</sup>');
  // Subscripts with braces and single character
  str = str.replace(/_{([^{}]+)}/g, '<sub>$1</sub>');
  str = str.replace(/_([a-zA-Z0-9+\-α-ωΑ-Ω])/g, '<sub>$1</sub>');
  return str;
}

function formatLatexMath(latex: string, isDisplay: boolean): string {
  let math = latex.trim();

  math = parseFractions(math);
  math = parseSqrt(math);
  math = parseScripts(math);

  math = math.replace(/\\text\{([^{}]+)\}/g, '<span class="math-text">$1</span>');
  math = math.replace(/\\mathbf\{([^{}]+)\}/g, '<strong>$1</strong>');
  math = math.replace(/\\mathit\{([^{}]+)\}/g, '<em>$1</em>');
  math = math.replace(/\\mathrm\{([^{}]+)\}/g, '<span class="math-rm">$1</span>');
  math = math.replace(/\\mathbb\{([^{}]+)\}/g, '<span class="math-bb">$1</span>');

  for (const [cmd, sym] of LATEX_SYMBOL_PAIRS) {
    math = math.split(cmd).join(sym);
  }

  math = math.replace(/\\,/g, '&thinsp;');
  math = math.replace(/\\;/g, '&ensp;');
  math = math.replace(/\\quad/g, '&emsp;');
  math = math.replace(/\\qquad/g, '&emsp;&emsp;');
  math = math.replace(/\\ /g, ' ');
  math = math.replace(/\\([a-zA-Z]+)/g, '$1');

  if (isDisplay) {
    return `<div class="math-block"><div class="math-inner">${math}</div></div>`;
  }
  return `<span class="math-inline">${math}</span>`;
}

function parseMarkdownTables(text: string): string {
  const lines = text.split('\n');
  const result: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i] ?? '';
    const nextLine = lines[i + 1] ?? '';

    if (
      line.trim().startsWith('|') &&
      line.trim().endsWith('|') &&
      nextLine.trim().startsWith('|') &&
      nextLine.includes('---')
    ) {
      const headerCols = line
        .trim()
        .slice(1, -1)
        .split('|')
        .map((c) => c.trim());
      i += 2; // skip header & separator line

      const rows: string[][] = [];
      while (i < lines.length) {
        const curLine = lines[i] ?? '';
        if (!curLine.trim().startsWith('|') || !curLine.trim().endsWith('|')) {
          break;
        }
        const rowCols = curLine
          .trim()
          .slice(1, -1)
          .split('|')
          .map((c) => c.trim());
        rows.push(rowCols);
        i++;
      }

      let tableHtml = '<div class="md-table-wrap"><table class="md-table"><thead><tr>';
      for (const col of headerCols) {
        tableHtml += `<th>${col}</th>`;
      }
      tableHtml += '</tr></thead><tbody>';
      for (const row of rows) {
        tableHtml += '<tr>';
        for (let c = 0; c < headerCols.length; c++) {
          tableHtml += `<td>${row[c] !== undefined ? row[c] : ''}</td>`;
        }
        tableHtml += '</tr>';
      }
      tableHtml += '</tbody></table></div>';
      result.push(tableHtml);
    } else {
      result.push(line);
      i++;
    }
  }

  return result.join('\n');
}

function parseMarkdownLists(text: string): string {
  const lines = text.split('\n');
  const result: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i] ?? '';
    const isUl = /^(\s*)[-*+]\s+(.*)$/.exec(line);
    const isOl = /^(\s*)\d+\.\s+(.*)$/.exec(line);

    if (isUl) {
      result.push('<ul class="md-list">');
      while (i < lines.length) {
        const cur = lines[i] ?? '';
        const ulMatch = /^(\s*)[-*+]\s+(.*)$/.exec(cur);
        if (!ulMatch) break;
        result.push(`<li>${ulMatch[2] ?? ''}</li>`);
        i++;
      }
      result.push('</ul>');
    } else if (isOl) {
      result.push('<ol class="md-list">');
      while (i < lines.length) {
        const cur = lines[i] ?? '';
        const olMatch = /^(\s*)\d+\.\s+(.*)$/.exec(cur);
        if (!olMatch) break;
        result.push(`<li>${olMatch[2] ?? ''}</li>`);
        i++;
      }
      result.push('</ol>');
    } else {
      result.push(line);
      i++;
    }
  }

  return result.join('\n');
}

function parseMarkdownBlockquotes(text: string): string {
  const lines = text.split('\n');
  const result: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i] ?? '';
    if (/^&gt;\s?(.*)$/.test(line)) {
      const bqLines: string[] = [];
      while (i < lines.length) {
        const cur = lines[i] ?? '';
        if (!/^&gt;\s?(.*)$/.test(cur)) break;
        const m = /^&gt;\s?(.*)$/.exec(cur);
        bqLines.push(m ? (m[1] ?? '') : '');
        i++;
      }
      result.push(`<blockquote class="md-blockquote"><p>${bqLines.join('<br />')}</p></blockquote>`);
    } else {
      result.push(line);
      i++;
    }
  }

  return result.join('\n');
}

export function renderMarkdownAndMath(rawText: string): string {
  if (!rawText) return '';

  const placeholders: Map<string, string> = new Map();
  let placeholderCounter = 0;

  const createPlaceholder = (content: string, prefix = 'PH'): string => {
    const id = `@@@${prefix}_${placeholderCounter++}_${Math.random().toString(36).substring(2, 7)}@@@`;
    placeholders.set(id, content);
    return id;
  };

  // Step 1: Pre-process Code Blocks (preserve raw formatting)
  let text = rawText;
  text = text.replace(/```([a-zA-Z0-9_\-#+.]*)\n?([\s\S]*?)(?:```|$)/g, (_match, lang, code) => {
    const cleanLang = (lang || '').trim().toLowerCase();
    const cleanCode = code.replace(/\n$/, '');
    const escapedCode = escapeHtml(cleanCode);
    const encodedForCopy = encodeURIComponent(cleanCode);
    const codeBlockHtml = `<div class="md-code-block"><div class="md-code-header"><span class="md-code-lang">${escapeHtml(cleanLang || 'code')}</span><button class="md-copy-btn" type="button" onclick="window.__copyPlaygroundCode(this)" data-code="${encodedForCopy}">Copy</button></div><pre><code class="language-${escapeHtml(cleanLang || 'plaintext')}">${escapedCode}</code></pre></div>`;
    return createPlaceholder(codeBlockHtml, 'CODE');
  });

  // Step 2: Pre-process Display Math ($$...$$ and \[...\])
  text = text.replace(/(?:\$\$|\\\[)([\s\S]*?)(?:\$\$|\\\]|$)/g, (_match, mathContent) => {
    if (!mathContent.trim()) return '';
    const formatted = formatLatexMath(mathContent, true);
    return createPlaceholder(formatted, 'MATH_DISP');
  });

  // Step 3: Pre-process Inline Math ($...$ and \(...\))
  text = text.replace(/\$([^\$\s](?:[^\$]*?[^\$\s])?)\$/g, (_match, mathContent) => {
    if (/^\d+(?:\.\d+)?$/.test(mathContent.trim())) {
      return `$${mathContent}$`;
    }
    const formatted = formatLatexMath(mathContent, false);
    return createPlaceholder(formatted, 'MATH_INL');
  });
  text = text.replace(/\\\(([\s\S]*?)\\\)/g, (_match, mathContent) => {
    const formatted = formatLatexMath(mathContent, false);
    return createPlaceholder(formatted, 'MATH_INL');
  });

  // Step 4: Pre-process Inline Code (`code`)
  text = text.replace(/`([^`]+)`/g, (_match, codeContent) => {
    const escaped = escapeHtml(codeContent);
    return createPlaceholder(`<code class="md-inline-code">${escaped}</code>`, 'INLINE_CODE');
  });

  // Step 5: Escape remaining text to guarantee HTML safety
  text = escapeHtml(text);

  // Step 6: Parse Markdown Tables
  text = parseMarkdownTables(text);

  // Step 7: Parse Headings
  text = text.replace(/^#### (.*$)/gm, '<h4 class="md-h4">$1</h4>');
  text = text.replace(/^### (.*$)/gm, '<h3 class="md-h3">$1</h3>');
  text = text.replace(/^## (.*$)/gm, '<h2 class="md-h2">$1</h2>');
  text = text.replace(/^# (.*$)/gm, '<h1 class="md-h1">$1</h1>');

  // Step 8: Parse Blockquotes
  text = parseMarkdownBlockquotes(text);

  // Step 9: Parse Horizontal Rules
  text = text.replace(/^(?:---|\*\*\*|___)\s*$/gm, '<hr class="md-hr" />');

  // Step 10: Parse Lists
  text = parseMarkdownLists(text);

  // Step 11: Parse Inline Markdown (Bold, Italic, Strikethrough, Links)
  text = text.replace(/\*\*(.*?)\*\*/g, '<strong>$1</strong>');
  text = text.replace(/__(.*?)__/g, '<strong>$1</strong>');
  text = text.replace(/(^|[^\*])\*([^\*]+)\*([^\*]|$)/g, '$1<em>$2</em>$3');
  text = text.replace(/(^|[^_])_([^_]+)_([^_]|$)/g, '$1<em>$2</em>$3');
  text = text.replace(/~~(.*?)~~/g, '<del>$1</del>');
  text = text.replace(
    /\[([^\]]+)\]\(([^)"]+)\)/g,
    '<a href="$2" target="_blank" rel="noopener noreferrer" class="md-link">$1</a>',
  );

  // Step 12: Paragraphs and Line Breaks
  const blocks = text.split(/\n\n+/);
  const formattedBlocks = blocks.map((block) => {
    const trimmed = block.trim();
    if (!trimmed) return '';
    if (
      trimmed.startsWith('<h1') ||
      trimmed.startsWith('<h2') ||
      trimmed.startsWith('<h3') ||
      trimmed.startsWith('<h4') ||
      trimmed.startsWith('<hr') ||
      trimmed.startsWith('<ul') ||
      trimmed.startsWith('<ol') ||
      trimmed.startsWith('<blockquote') ||
      trimmed.startsWith('<div class="md-table-wrap"') ||
      trimmed.startsWith('@@@CODE_') ||
      trimmed.startsWith('@@@MATH_DISP_')
    ) {
      return trimmed;
    }
    const withBreaks = trimmed.replace(/\n/g, '<br />');
    return `<p class="md-p">${withBreaks}</p>`;
  });

  let result = formattedBlocks.filter(Boolean).join('\n');

  // Step 13: Restore all Placeholders
  for (const [id, originalHtml] of placeholders.entries()) {
    result = result.split(id).join(originalHtml);
  }

  return result;
}