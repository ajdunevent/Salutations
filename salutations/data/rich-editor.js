// The rich text editor's page script, injected by the composer. It reports
// the formatting at the cursor and the body's HTML, and runs the commands
// the formatting bar sends.
(function () {
    'use strict';
    const handler = window.webkit.messageHandlers.salutations;
    const post = (m) => handler.postMessage(m);
    const body = document.body;
    const SIG = 'salutations-signature';

    document.execCommand('styleWithCSS', false, true);
    document.execCommand('defaultParagraphSeparator', false, 'div');

    const q = (c) => { try { return document.queryCommandState(c); } catch (e) { return false; } };
    const v = (c) => { try { return document.queryCommandValue(c) || ''; } catch (e) { return ''; } };

    function blockOf(node) {
        while (node && node !== body) {
            if (node.nodeType === 1 && /^(DIV|P|LI|BLOCKQUOTE|TD|TH|H[1-6])$/.test(node.nodeName)) return node;
            node = node.parentNode;
        }
        return null;
    }

    function state() {
        let align = 'left';
        if (q('justifyCenter')) align = 'center';
        else if (q('justifyRight')) align = 'right';
        else if (q('justifyFull')) align = 'justify';
        let list = '';
        if (q('insertUnorderedList')) list = 'bullet';
        else if (q('insertOrderedList')) list = 'number';
        const sel = window.getSelection();
        let el = sel && sel.anchorNode;
        if (el && el.nodeType !== 1) el = el.parentNode;
        const px = el ? parseFloat(getComputedStyle(el).fontSize) : 0;
        const r = px / parseFloat(getComputedStyle(document.documentElement).fontSize);
        const size = !r || (r > 0.9 && r < 1.15) ? 'normal' : r <= 0.9 ? 'small' : r < 1.7 ? 'large' : 'huge';
        const color = v('foreColor');
        const auto = color === getComputedStyle(body).color;
        let highlight = v('backColor');
        if (/rgba\(\s*0,\s*0,\s*0,\s*0\s*\)|transparent/.test(highlight)) highlight = '';
        return {
            t: 'state',
            bold: q('bold'), italic: q('italic'), underline: q('underline'), strike: q('strikeThrough'),
            mono: /monospace|courier/i.test(v('fontName')),
            size, color: auto ? '' : color, highlight, align, list,
        };
    }

    let last = '';
    function report() {
        const s = state();
        const key = JSON.stringify(s);
        if (key !== last) {
            last = key;
            post(s);
        }
    }

    function changed() {
        post({ t: 'html', html: body.innerHTML });
        report();
    }

    function signatureEdited() {
        const sel = window.getSelection();
        let node = sel && sel.anchorNode;
        while (node && node !== body) {
            if (node.nodeType === 1 && node.classList.contains(SIG)) {
                node.classList.remove(SIG);
                post({ t: 'sig' });
                return;
            }
            node = node.parentNode;
        }
    }

    // Markdown typed as it is in plain text becomes formatting.
    const INLINE = [
        [/\*\*([^*\s](?:[^*]*[^*\s])?)\*\*$/, ['bold']],
        [/~~([^~\s](?:[^~]*[^~\s])?)~~$/, ['strikeThrough']],
        [/(?:^|[\s(])\*([^*\s](?:[^*]*[^*\s])?)\*$/, ['italic']],
        [/(?:^|[\s(])_([^_\s](?:[^_]*[^_\s])?)_$/, ['italic']],
        [/`([^`]+)`$/, ['mono']],
    ];

    function autoformatInline() {
        const sel = window.getSelection();
        if (!sel.isCollapsed || sel.anchorNode.nodeType !== 3) return false;
        const node = sel.anchorNode;
        const before = node.data.slice(0, sel.anchorOffset);
        for (const [re, cmds] of INLINE) {
            const m = re.exec(before);
            if (!m) continue;
            const whole = m[0].trimStart().replace(/^\(/, '');
            const inner = m[1];
            const start = sel.anchorOffset - whole.length;
            const range = document.createRange();
            range.setStart(node, start);
            range.setEnd(node, sel.anchorOffset);
            sel.removeAllRanges();
            sel.addRange(range);
            document.execCommand('insertText', false, inner);
            const after = sel.anchorNode, end = sel.anchorOffset;
            const r2 = document.createRange();
            r2.setStart(after, end - inner.length);
            r2.setEnd(after, end);
            sel.removeAllRanges();
            sel.addRange(r2);
            for (const c of cmds) apply(c, true);
            sel.collapseToEnd();
            for (const c of cmds) apply(c, false);
            return true;
        }
        return false;
    }

    function autoformatBlock() {
        const sel = window.getSelection();
        if (!sel.isCollapsed || sel.anchorNode.nodeType !== 3) return false;
        const node = sel.anchorNode;
        const block = blockOf(node);
        const first = block ? block.firstChild : body.firstChild;
        if (node !== first && !(block && block.firstChild === node)) return false;
        const before = node.data.slice(0, sel.anchorOffset);
        const actions = {
            '- ': () => document.execCommand('insertUnorderedList'),
            '* ': () => document.execCommand('insertUnorderedList'),
            '1. ': () => document.execCommand('insertOrderedList'),
            '> ': () => document.execCommand('formatBlock', false, 'blockquote'),
            '# ': () => { setSize('huge'); apply('bold', true); },
            '## ': () => { setSize('large'); apply('bold', true); },
        };
        const act = actions[before.replace(/\u00a0/g, ' ')];
        if (!act) return false;
        const range = document.createRange();
        range.setStart(node, 0);
        range.setEnd(node, sel.anchorOffset);
        sel.removeAllRanges();
        sel.addRange(range);
        document.execCommand('delete');
        act();
        return true;
    }

    body.addEventListener('input', (e) => {
        signatureEdited();
        changed();
        if (e.inputType !== 'insertText' || !e.data) return;
        // Not during the typing command, which resets the typing style
        // as it finishes.
        const space = e.data === ' ' || e.data === '\u00a0';
        const marker = /[*_~`]/.test(e.data);
        if (space || marker) {
            setTimeout(() => {
                const done = (space && autoformatBlock()) || (marker && autoformatInline());
                if (done) changed();
            }, 0);
        }
    });
    document.addEventListener('selectionchange', report);
    // A click in the empty space under the signature goes to the end of
    // the text above it, not into the signature.
    body.addEventListener('mouseup', (e) => {
        const sig = body.querySelector('.' + SIG);
        if (e.target !== body || !sig || !window.getSelection().isCollapsed) return;
        if (e.clientY < sig.getBoundingClientRect().bottom) return;
        let to = sig.previousElementSibling;
        if (to && to.innerHTML === '<br>' && to.previousElementSibling) to = to.previousElementSibling;
        if (!to) return;
        const r = document.createRange();
        r.selectNodeContents(to);
        r.collapse(false);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(r);
    });
    document.addEventListener('keydown', (e) => {
        // Enter on an empty line in a quote leaves the quote.
        if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey) {
            const sel = window.getSelection();
            const block = sel.isCollapsed ? blockOf(sel.anchorNode) : null;
            let quote = block;
            while (quote && quote !== body && quote.nodeName !== 'BLOCKQUOTE') quote = quote.parentNode;
            if (quote && quote !== body && block.textContent === '' && !block.closest('li')) {
                e.preventDefault();
                document.execCommand(block === quote ? 'formatBlock' : 'outdent', false, 'div');
                changed();
                return;
            }
        }
        // Tab moves between table cells and nests list items.
        if (e.key === 'Tab' && !e.ctrlKey && !e.altKey && !e.metaKey) {
            const sel = window.getSelection();
            let n = sel.anchorNode;
            while (n && n !== body && !(n.nodeType === 1 && /^(TD|TH|LI)$/.test(n.nodeName))) n = n.parentNode;
            if (n && n !== body && n.nodeName === 'LI') {
                e.preventDefault();
                document.execCommand(e.shiftKey ? 'outdent' : 'indent');
                changed();
                return;
            }
            if (n && n !== body) {
                e.preventDefault();
                const cells = [...n.closest('table').querySelectorAll('td, th')];
                const next = cells[cells.indexOf(n) + (e.shiftKey ? -1 : 1)];
                if (next) {
                    const r = document.createRange();
                    r.selectNodeContents(next);
                    sel.removeAllRanges();
                    sel.addRange(r);
                }
                return;
            }
        }
        if (!e.ctrlKey || e.altKey || e.shiftKey || e.metaKey) return;
        const cmd = { b: 'bold', i: 'italic', u: 'underline' }[e.key.toLowerCase()];
        if (!cmd) return;
        e.preventDefault();
        apply(cmd, !q(cmd));
        changed();
    });

    function apply(cmd, on) {
        if (cmd === 'mono') {
            const now = /monospace|courier/i.test(v('fontName'));
            if (now !== on) document.execCommand('fontName', false, on ? 'monospace' : 'inherit');
            return;
        }
        if (q(cmd) !== on) document.execCommand(cmd, false, null);
    }

    // Sizes are relative to the reader's own text size.
    const SIZES = { small: '0.8rem', normal: '', large: '1.4rem', huge: '2rem' };

    function setSize(key) {
        document.execCommand('fontSize', false, '7');
        for (const el of body.querySelectorAll('font[size="7"], [style*="xxx-large"]')) {
            el.removeAttribute('size');
            el.style.fontSize = SIZES[key] || '';
            if (!el.getAttribute('style')) el.removeAttribute('style');
        }
    }

    function escapeHtml(s) {
        return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
    }

    function run(f) {
        body.focus();
        f();
        changed();
    }

    window.salutations = {
        style: (cmd, on) => run(() => apply(cmd, on)),
        normal: () => run(() => {
            document.execCommand('removeFormat');
            apply('mono', false);
        }),
        exec: (cmd, value) => run(() => document.execCommand(cmd, false, value)),
        size: (key) => run(() => setSize(key)),
        color: (c) => run(() => document.execCommand('foreColor', false, c || 'inherit')),
        highlight: (c) => run(() => document.execCommand('hiliteColor', false, c || 'transparent')),
        list: (kind) => run(() => document.execCommand(kind === 'number' ? 'insertOrderedList' : 'insertUnorderedList')),
        link: (url, text) => run(() => {
            const sel = window.getSelection();
            if (sel && !sel.isCollapsed) {
                document.execCommand('createLink', false, url);
            } else {
                document.execCommand('insertHTML', false, '<a href="' + escapeHtml(url) + '">' + escapeHtml(text || url) + '</a>');
            }
        }),
        table: (cols, rows, header) => run(() => {
            const cell = (tag) => '<' + tag + ' style="border:1px solid #888;padding:4px 8px;min-width:3em"><br></' + tag + '>';
            let html = '<table style="border-collapse:collapse"><tbody>';
            for (let r = 0; r < rows; r++) {
                html += '<tr>' + Array(cols).fill(cell(header && r === 0 ? 'th' : 'td')).join('') + '</tr>';
            }
            document.execCommand('insertHTML', false, html + '</tbody></table><div><br></div>');
        }),
        image: (cid, name, width) => run(() => {
            const w = width ? ' width="' + width + '"' : '';
            document.execCommand('insertHTML', false, '<img src="cid:' + escapeHtml(cid) + '" alt="' + escapeHtml(name) + '"' + w + ' style="max-width:100%;height:auto">');
        }),
        // The signature, already marked, or none.
        signature: (html, above) => {
            for (const old of document.querySelectorAll('.' + SIG)) {
                // The blank line put before or after it goes with it.
                for (const n of [old.previousElementSibling, old.nextElementSibling]) {
                    if (n && n.nodeName === 'DIV' && n.innerHTML === '<br>') { n.remove(); break; }
                }
                old.remove();
            }
            if (html) {
                const holder = document.createElement('div');
                holder.innerHTML = html;
                const sig = holder.firstElementChild;
                let marker = null;
                if (above) {
                    for (const d of body.querySelectorAll('div')) {
                        if (/^--- (Previous|Forwarded) Message ---$/.test(d.textContent)) { marker = d; break; }
                    }
                }
                if (marker) {
                    body.insertBefore(sig, marker);
                    body.insertBefore(document.createElement('div'), marker).innerHTML = '<br>';
                } else {
                    body.appendChild(document.createElement('div')).innerHTML = '<br>';
                    body.appendChild(sig);
                }
            }
            changed();
        },
        theme: (css) => { document.getElementById('salutations-theme').textContent = css; },
        caretToStart: () => {
            const sel = window.getSelection();
            const r = document.createRange();
            r.setStart(body, 0);
            r.collapse(true);
            sel.removeAllRanges();
            sel.addRange(r);
            body.focus();
        },
        focus: () => body.focus(),
    };
    changed();
})();
