'use strict';
// Preserve the existing fetch contract while dispatching API requests in process.
if (window.__TAURI__) {
  const invoke = window.__TAURI__.core.invoke;
  const browserFetch = window.fetch.bind(window);
  window.fetch = async (input, init = {}) => {
    const path = typeof input === 'string' ? input : input.url;
    if (path.startsWith('/api/') && !path.startsWith('/api/thumb/')) {
      const result = await invoke('api_request', {path, method: init.method || 'GET', body: init.body || null});
      return new Response(new Uint8Array(result.body), {status: result.status, headers: result.headers});
    }
    return browserFetch(input, init);
  };
  window.curatorFileUrl = path => {
    const base = navigator.userAgent.includes('Windows') ? 'http://curator.localhost' : 'curator://localhost';
    return base + path;
  };
  // Thumbnails created in older UI code retain their familiar relative URLs.
  const rewrite = node => {
    if (node.nodeType !== 1) return;
    for (const element of [node, ...node.querySelectorAll('img,video,source')]) {
      const src = element.getAttribute('src');
      if (src && (src.startsWith('/library/') || src.startsWith('/api/thumb/'))) element.src = window.curatorFileUrl(src);
    }
  };
  new MutationObserver(changes => changes.forEach(c => {
    if (c.type === 'attributes') rewrite(c.target);
    else c.addedNodes.forEach(rewrite);
  })).observe(document.documentElement, {subtree:true,childList:true,attributes:true,attributeFilter:['src']});
  window.__TAURI__.event.listen('desktop-menu', ({payload}) => {
    document.querySelector(payload === 'settings' ? '#settings-btn' : '#add-source-btn')?.click();
  });
  document.addEventListener('DOMContentLoaded',()=>{
    for(const [id,directory] of [['data-dir-input',true],['dep-gallery_dl-path',false],['dep-ffprobe-path',false]]) {
      const input=document.getElementById(id);if(!input)continue;
      const button=document.createElement('button');button.type='button';button.textContent='Browse…';button.className='btn btn-ghost';
      button.onclick=async()=>{const path=await invoke('choose_path',{directory});if(path){input.value=path;input.dispatchEvent(new Event('input'));}};
      input.after(button);
    }
  });
}
