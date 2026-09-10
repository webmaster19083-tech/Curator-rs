function feedBuildReviewControls(section, item) {
  const card = document.createElement('div'); card.className = 'feed-review-card';
  while (section.firstChild) card.appendChild(section.firstChild);
  section.appendChild(card);
  const stamp = document.createElement('div'); stamp.className = 'feed-swipe-stamp';
  stamp.setAttribute('aria-hidden', 'true'); card.appendChild(stamp);
  const panel = document.createElement('div'); panel.className = 'feed-review-controls';
  const label = document.createElement('div'); label.textContent = `AUTO ${item.auto_rating} - Needs review`;
  panel.appendChild(label);
  let saving = false, start = null, suppressClick = false;
  const resetDrag = () => {
    card.style.transform = ''; card.classList.remove('dragging');
    stamp.style.opacity = '0';
  };
  const animate = async (direction) => {
    resetDrag();
    if (!card.animate || window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    const target = direction === 'approve' ? 'translateX(110%) rotate(14deg)' :
      direction === 'next' ? 'translateY(-35%) scale(.94)' : 'translateX(-24px) rotate(-2deg)';
    const animation = card.animate([
      {transform:'none', opacity:1},
      {transform:target, opacity:direction === 'choose' ? 1 : 0}
    ], {duration:direction === 'choose' ? 180 : 260, easing:'ease-out', direction:direction === 'choose' ? 'alternate' : 'normal', iterations:direction === 'choose' ? 2 : 1});
    try { await animation.finished; } catch (_) {}
  };
  const save = async rating => {
    if (saving || item.rating_reviewed) { resetDrag(); return; }
    saving = true;
    const session = feed.session;
    panel.querySelectorAll('button').forEach(b => b.disabled = true);
    resetDrag();
    try {
      const result = await api(`/api/media/${item.id}/rating${rating == null ? '/approve' : ''}`, {
        method:rating == null ? 'POST' : 'PUT', ...(rating == null ? {} : {body:JSON.stringify({rating})})
      });
      Object.assign(item, result);
      const original = state.currentItems.find(m => m.id === item.id);
      if (original) Object.assign(original, result);
      if (!feed.active || session !== feed.session) return;
      label.textContent = `AUTO ${item.auto_rating} -> HUMAN ${item.rating}`;
      refreshStars(item.rating);
      toast(label.textContent);
      await animate(rating == null ? 'approve' : 'next');
      if (feed.active && session === feed.session && feed.activeSection === section) feedGoNext(section);
    } catch (e) {
      toast('Could not save rating: ' + e.message, true);
      panel.querySelectorAll('button').forEach(b => b.disabled = false);
    } finally { saving = false; }
  };
  const stars = document.createElement('div'); stars.className = 'feed-review-stars';
  stars.setAttribute('role', 'group'); stars.setAttribute('aria-label', 'Choose human rating');
  const choices = [];
  const refreshStars = rating => choices.forEach((b, i) => {
    b.classList.toggle('filled', i < rating);
    b.setAttribute('aria-pressed', String(i + 1 === rating));
  });
  for (let r = 1; r <= 5; r++) {
    const b = document.createElement('button'); b.className = 'star'; b.textContent = '\u2605';
    b.setAttribute('aria-label', `Rate ${r} ${r === 1 ? 'star' : 'stars'}`);
    b.title = `${r} / 5`; b.addEventListener('click', () => save(r));
    choices.push(b); stars.appendChild(b);
  }
  refreshStars(item.auto_rating); panel.appendChild(stars);
  const button = (text, action) => {
    const b = document.createElement('button'); b.className = 'btn'; b.textContent = text;
    b.addEventListener('click', action); panel.appendChild(b); return b;
  };
  button('Approve', () => save(null));
  button('Skip', async () => {
    if (saving) return;
    saving = true;
    const session = feed.session;
    await animate('next');
    if (feed.active && session === feed.session && feed.activeSection === section) feedGoNext(section);
    saving = false;
  });
  const hint = document.createElement('small'); hint.textContent = 'Swipe right: approve / left: choose stars / up: skip';
  panel.appendChild(hint); card.appendChild(panel);
  section.addEventListener('click', e => {
    if (suppressClick) { e.preventDefault(); e.stopPropagation(); suppressClick = false; }
  }, true);
  section.addEventListener('pointerdown', e => {
    suppressClick = false;
    if (saving || !e.isPrimary || e.target.closest('button, .feed-progress')) return;
    start = {x:e.clientX, y:e.clientY};
  });
  section.addEventListener('pointermove', e => {
    if (!start || saving) return;
    const dx = e.clientX - start.x, dy = e.clientY - start.y;
    if (Math.abs(dx) < 8 || Math.abs(dx) < Math.abs(dy) * 1.5) return;
    section.setPointerCapture(e.pointerId);
    suppressClick = true; card.classList.add('dragging');
    const shift = Math.max(-180, Math.min(180, dx));
    card.style.transform = `translateX(${shift}px) rotate(${shift / 22}deg)`;
    stamp.textContent = dx > 0 ? 'APPROVE' : 'CHOOSE STARS';
    stamp.classList.toggle('choose', dx < 0);
    stamp.style.opacity = String(Math.min(1, Math.abs(dx) / 90));
  });
  section.addEventListener('pointercancel', () => { start = null; resetDrag(); });
  section.addEventListener('pointerup', e => {
    if (!start) return;
    const dx = e.clientX - start.x, dy = e.clientY - start.y; start = null;
    if (section.hasPointerCapture(e.pointerId)) section.releasePointerCapture(e.pointerId);
    resetDrag();
    if (saving || Math.abs(dx) < 60 || Math.abs(dx) < Math.abs(dy) * 1.5) return;
    suppressClick = true;
    if (dx > 0) save(null);
    else {
      choices[0].focus(); label.textContent = `AUTO ${item.auto_rating} - Choose a human star rating`;
      animate('choose');
    }
  });
}

