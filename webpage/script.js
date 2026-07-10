// ── Install tabs ──────────────────────────────────────────
document.querySelectorAll('.tab').forEach(tab => {
    tab.addEventListener('click', () => {
        document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
        document.querySelectorAll('.install-content').forEach(c => c.classList.remove('active'));
        tab.classList.add('active');
        const target = document.getElementById('tab-' + tab.dataset.tab);
        if (target) target.classList.add('active');
    });
});

// ── Navbar scroll effect ──────────────────────────────────
const navbar = document.getElementById('navbar');
window.addEventListener('scroll', () => {
    navbar.classList.toggle('scrolled', window.scrollY > 40);
}, { passive: true });

// ── Reveal on scroll ──────────────────────────────────────
const revealElements = document.querySelectorAll(
    '.feature-card, .step, .config-block, .terminal, .screenshot-card, .video-wrapper, .ledger-wrap, .faq-item'
);
const revealObserver = new IntersectionObserver((entries) => {
    entries.forEach(entry => {
        if (entry.isIntersecting) {
            entry.target.classList.add('revealed');
            revealObserver.unobserve(entry.target);
        }
    });
}, { threshold: 0.1 });
revealElements.forEach(el => { el.classList.add('reveal'); revealObserver.observe(el); });

// ── Broken image fallback ─────────────────────────────────
document.querySelectorAll('.screenshot-card img').forEach(img => {
    img.addEventListener('error', () => {
        const placeholder = document.createElement('div');
        placeholder.className = 'img-error';
        const card = img.closest('.screenshot-card');
        const label = card && card.querySelector('.screenshot-label');
        placeholder.innerHTML = (label ? label.textContent.trim() : 'Screenshot')
            + '<span class="img-error-sub">preview coming soon</span>';
        img.replaceWith(placeholder);
    });
});

// ── Real flip data: ticker tape + ledger table ────────────
// flips.json is an anonymized sample of 5M+ profit snipes. The tape scrolls
// a shuffled reel; the ledger table shows the biggest ones, sorted by profit.
(function () {
    // True SkyBlock rarity colours for item names (readable on dark).
    const RCOLOR = { common:'#f5f5f5', uncommon:'#5cff5c', rare:'#5b8cff', epic:'#c45bff',
        legendary:'#ffb627', mythic:'#ff66e0', divine:'#4fe0ff', special:'#ff6b6b',
        very_special:'#ff6b6b', supreme:'#ff6b6b', ultimate:'#ff4d4d' };
    const ICON = (tag) => 'https://sky.coflnet.com/static/icon/' + encodeURIComponent(tag);

    const fmt = (n) => {
        n = +n || 0; const a = Math.abs(n);
        if (a >= 1e9) return (n/1e9).toFixed(2)+'B';
        if (a >= 1e6) return (n/1e6).toFixed(1)+'M';
        if (a >= 1e3) return (n/1e3).toFixed(0)+'K';
        return ''+n;
    };
    const esc = (s) => String(s).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));

    fetch('flips.json').then(r => r.ok ? r.json() : Promise.reject()).then(data => {
        if (!Array.isArray(data) || data.length < 3) return;

        // ── ticker tape (shuffled reel, content doubled for a seamless loop) ──
        const tape = document.getElementById('tape');
        const track = document.getElementById('tapetrack');
        if (tape && track) {
            const reel = data.slice();
            for (let i = reel.length-1; i > 0; i--){ const j = Math.floor(Math.random()*(i+1)); [reel[i],reel[j]]=[reel[j],reel[i]]; }
            const items = reel.slice(0, 24).map(f => {
                const col = RCOLOR[f.rarity] || '#e9e1cc';
                return `<span class="tp"><span class="tp-item" style="color:${col}">${esc(f.item)}</span>`
                     + `<span class="tp-profit">+${fmt(f.profit)}</span>`
                     + `<span class="tp-ms">${esc(f.speed)}ms</span></span>`;
            }).join('');
            track.innerHTML = items + items;
            // ~4s per flip keeps the tape readable regardless of item count
            track.style.animationDuration = Math.round(reel.slice(0, 24).length * 4) + 's';
            tape.hidden = false;
        }

        // ── ledger table (top flips by profit) ──
        const body = document.getElementById('ledger-body');
        const section = document.getElementById('live');
        if (body && section) {
            const top = data.slice().sort((a,b) => b.profit - a.profit).slice(0, 10);
            body.innerHTML = top.map(f => {
                const col = RCOLOR[f.rarity] || '#e9e1cc';
                const icon = f.tag ? `<img src="${ICON(f.tag)}" loading="lazy" alt="" onerror="this.remove()">` : '';
                return `<tr>
                    <td class="l-item" style="color:${col}">${icon}${esc(f.item)}</td>
                    <td class="l-finder l-finder-col">${esc(f.finder)}</td>
                    <td class="num">${fmt(f.price)}</td>
                    <td class="num">${fmt(f.target)}</td>
                    <td class="num l-profit">+${fmt(f.profit)}<small>${esc(f.roi)}%</small></td>
                    <td class="num">${esc(f.speed)}ms</td>
                </tr>`;
            }).join('');
            section.hidden = false;
        }
    }).catch(() => { /* no data — tape and ledger stay hidden */ });
})();
