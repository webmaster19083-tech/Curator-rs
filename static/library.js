'use strict';
function formatBytes(value) {
  if (value == null) return 'Unknown';
  if (value === 0) return '0 B';
  const unit = Math.min(4, Math.floor(Math.log(value) / Math.log(1024)));
  return `${(value / 1024 ** unit).toLocaleString(undefined, {maximumFractionDigits:unit ? 1 : 0})} ${['B','KB','MB','GB','TB'][unit]}`;
}
let libraryTotals = {groups:[], sources:[]};
const originalRefreshGroups = refreshGroups;
refreshGroups = async function() {
  await originalRefreshGroups();
  libraryTotals = window.__TAURI__ ? await window.__TAURI__.core.invoke('library_summary') : await api('/api/library/summary');
};
function libraryButton(label, action) {
  const button = document.createElement('button');
  button.type = 'button'; button.className = 'library-chip'; button.textContent = label;
  button.addEventListener('click', e => { e.stopPropagation(); Promise.resolve(action(e)).catch(error => toast(error.message,true)); });
  return button;
}
function libraryDialog(title) {
  const dialog = document.createElement('dialog'); dialog.className='library-dialog';
  const heading = document.createElement('h3'); heading.textContent=title; dialog.append(heading);
  const close = libraryButton('Close', () => dialog.close());
  dialog.append(close); document.body.append(dialog); dialog.showModal();
  dialog.addEventListener('close',()=>dialog.remove()); return dialog;
}
async function organizeInto(groupId) {
  const dialog=libraryDialog('Add to Group');
  if(window.__TAURI__)dialog.append(libraryButton('Add Local Folder',async()=>{
    await window.__TAURI__.core.invoke('import_local_folder',{groupId});await refreshSources();await refreshGroups();renderSidebar();dialog.close();
  }));
  dialog.append(libraryButton('Create Subgroup',()=>{dialog.close(); return createGroup(undefined,groupId);}));
  dialog.append(libraryButton('Add New Source',()=>{dialog.close(); document.querySelector('#add-source-btn').click(); toast('Add the source, then use Add Existing Source to assign it here.');}));
  dialog.append(libraryButton('Move Selected Sources Here',async()=>{
    for(const source of state.sources.filter(s=>s.included)) await api(`/api/sources/${source.id}/group`,{method:'PATCH',body:JSON.stringify({group_id:groupId})});
    await refreshSources(); await refreshGroups(); renderSidebar(); dialog.close();
  }));
  const search=document.createElement('input'); search.placeholder='Find an existing source or group'; dialog.append(search);
  const results=document.createElement('div'); results.className='library-picker'; dialog.append(results);
  const render=()=>{
    results.replaceChildren();
    const needle=search.value.toLowerCase();
    const descendants=new Set([groupId]);
    let changed=true; while(changed){changed=false; for(const g of state.groups) if(descendants.has(g.parent_id)&&!descendants.has(g.id)){descendants.add(g.id);changed=true;}}
    for(const source of state.sources.filter(s=>s.name.toLowerCase().includes(needle)&&s.group_id!==groupId)) results.append(libraryButton(`Source · ${source.name}`,async()=>{
      await api(`/api/sources/${source.id}/group`,{method:'PATCH',body:JSON.stringify({group_id:groupId})});
      await refreshSources(); await refreshGroups(); renderSidebar(); dialog.close();
    }));
    // A group cannot be inserted beneath itself or one of its descendants.
    for(const g of state.groups.filter(g=>g.id!==groupId&&g.name.toLowerCase().includes(needle))) {
      let ancestor=state.groupsById[groupId]; const seen=new Set(); let invalid=false;
      while(ancestor&&!seen.has(ancestor.id)){seen.add(ancestor.id);if(ancestor.id===g.id)invalid=true;ancestor=state.groupsById[ancestor.parent_id];}
      if(invalid)continue;
      results.append(libraryButton(`Group · ${g.name}`,async()=>{
        await api(`/api/groups/${g.id}`,{method:'PATCH',body:JSON.stringify({parent_id:groupId})});
        await refreshGroups(); renderSidebar(); dialog.close();
      }));
    }
  }; search.oninput=render; render();
}
async function groupTagPicker(id) {
  const dialog=libraryDialog('Add Tag');
  const search=document.createElement('input'); search.placeholder='Search tags or type a new tag'; dialog.append(search);
  const results=document.createElement('div'); results.className='library-picker'; dialog.append(results);
  const data=await api('/api/tags'); const chosen=new Set();
  const render=()=>{
    results.replaceChildren();
    for(const tag of data.tags.filter(t=>t.name.toLowerCase().includes(search.value.toLowerCase()))) {
      const label=document.createElement('label'); const check=document.createElement('input'); check.type='checkbox';
      check.checked=chosen.has(tag.name); check.onchange=()=>check.checked?chosen.add(tag.name):chosen.delete(tag.name);
      label.append(check,document.createTextNode(tag.name)); results.append(label);
    }
  };
  search.oninput=render; render();
  dialog.append(libraryButton('Create and select typed tag',()=>{if(search.value.trim()){chosen.add(search.value.trim());toast(`Selected ${search.value.trim()}`);}}));
  dialog.append(libraryButton('Add selected tags',async()=>{
    for(const name of chosen) await api(`/api/groups/${id}/tags`,{method:'POST',body:JSON.stringify({name})});
    await refreshGroups(); renderSidebar(); dialog.close();
  }));
}
function effectiveGroupTags(group) {
  const result=new Map(); const seen=new Set(); let current=group;
  while(current&&!seen.has(current.id)) {
    seen.add(current.id); for(const tag of current.tags||[]) if(!result.has(tag))result.set(tag,current.id!==group.id);
    current=state.groupsById[current.parent_id];
  }
  return [...result].map(([name,inherited])=>({name,inherited}));
}
function tagFilterButton(tag, groupId) {
  const button=libraryButton(tag.name+(tag.inherited?' ↗':''),()=>{
    const select=document.querySelector('#tag-filter-select');
    if(![...select.options].some(o=>o.value===tag.name))select.add(new Option(tag.name,tag.name));
    select.value=tag.name; select.dispatchEvent(new Event('change'));
  });
  button.title=tag.inherited?'Inherited from an ancestor':'Click to filter; right-click to remove from this group';
  if(!tag.inherited)button.oncontextmenu=e=>{e.preventDefault();e.stopPropagation();const d=libraryDialog(tag.name);d.append(libraryButton('Remove from this group',async()=>{await removeTagFromGroup(groupId,tag.name);d.close();}));};
  return button;
}
const originalRenderSidebar=renderSidebar;
renderSidebar=function(){
  originalRenderSidebar();
  const sourceTotals=new Map(libraryTotals.sources.map(s=>[s.id,s]));
  const groupTotals=new Map(libraryTotals.groups.map(g=>[g.id,g]));
  for(const row of document.querySelectorAll('.source-item[data-id]')) {
    const id=Number(row.dataset.id), total=sourceTotals.get(id); row.draggable=true;
    row.ondragstart=e=>e.dataTransfer.setData('application/x-curator',JSON.stringify({type:'source',id}));
    if(total)row.querySelector('.source-meta').append(document.createTextNode(` · ${formatBytes(total.bytes)}${total.unknown?' + unknown':''}`));
    row.oncontextmenu=e=>{e.preventDefault();toggleSourceMenu(id,row.querySelector('.source-menu-btn'));};
  }
  for(const row of document.querySelectorAll('.group-header[data-group-key]')) {
    const id=Number(row.dataset.groupKey), group=state.groupsById[id]; if(!group)continue;
    const total=groupTotals.get(id); const count=row.querySelector('.group-count');
    if(total)count.textContent=`${total.items.toLocaleString()} · ${formatBytes(total.bytes)}${total.unknown?' + unknown':''}`;
    row.querySelector('.group-tag-row')?.remove();
    const tags=effectiveGroupTags(group); const controls=document.createElement('div');controls.className='library-inline';
    tags.slice(0,3).forEach(tag=>controls.append(tagFilterButton(tag,id)));
    if(tags.length>3)controls.append(libraryButton(`+${tags.length-3}`,()=>{const d=libraryDialog('All group tags');tags.forEach(tag=>d.append(tagFilterButton(tag,id)));}));
    controls.append(libraryButton('Add Tag',()=>groupTagPicker(id))); row.append(controls);
    const add=document.createElement('div');add.className='library-inline';add.append(libraryButton('+... Add to Group',()=>organizeInto(id)));row.append(add);
    row.draggable=true;row.ondragstart=e=>e.dataTransfer.setData('application/x-curator',JSON.stringify({type:'group',id}));
    row.ondragover=e=>{if(e.dataTransfer.types.includes('application/x-curator'))e.preventDefault();};
    row.ondrop=async e=>{
      e.preventDefault();try {
        const item=JSON.parse(e.dataTransfer.getData('application/x-curator'));
        await api(item.type==='source'?`/api/sources/${item.id}/group`:`/api/groups/${item.id}`,{method:'PATCH',body:JSON.stringify(item.type==='source'?{group_id:id}:{parent_id:id})});
        await refreshSources();await refreshGroups();renderSidebar();
      }catch(error){toast(error.message,true);}
    };
    row.oncontextmenu=e=>{e.preventDefault();organizeInto(id);};
  }
  renderLibrarySummary();
};
function renderLibrarySummary(){
  let header=document.querySelector('#library-summary');
  if(!header){header=document.createElement('section');header.id='library-summary';document.querySelector('.toolbar').after(header);}
  const view=state.view; let group=view.type==='group'?state.groupsById[view.id]:null;
  const source=view.type==='creator'?state.sourcesById[view.id]:null;
  if(source)group=state.groupsById[source.group_id];
  const crumbs=[];const seen=new Set();while(group&&!seen.has(group.id)){seen.add(group.id);crumbs.unshift(group.name);group=state.groupsById[group.parent_id];}
  if(source)crumbs.push(source.name);
  const total=(source?libraryTotals.sources:libraryTotals.groups).find(t=>t.id===view.id);
  header.textContent=`Library${crumbs.length?' › '+crumbs.join(' › '):''}${total?' — '+total.items.toLocaleString()+' files · '+formatBytes(total.bytes)+(total.unknown?' · '+total.unknown+' unknown sizes':''):''}`;
}
document.addEventListener('keydown',e=>{
  if((e.ctrlKey||e.metaKey)&&e.key===','){e.preventDefault();document.querySelector('#settings-btn').click();}
  if(e.key==='F2'){
    const row=document.querySelector('.source-item.active .source-name,.group-header.active .group-name');
    if(row){e.preventDefault();row.dispatchEvent(new MouseEvent('dblclick',{bubbles:true}));}
  }
});
const sortSelect=document.querySelector('#sort-select');
sortSelect.add(new Option('Largest files','size_desc'));sortSelect.add(new Option('Smallest files','size_asc'));
const sidebar=document.querySelector('.sidebar');
sidebar.style.width=localStorage.getItem('library-sidebar-width')||'420px';
new ResizeObserver(()=>localStorage.setItem('library-sidebar-width',sidebar.style.width)).observe(sidebar);

const originalBuildTile=buildTile;
buildTile=function(item,index){
  const tile=originalBuildTile(item,index);
  const size=document.createElement('span');size.className='tile-size';size.textContent=formatBytes(item.file_size_bytes);tile.append(size);
  tile.title=`${item.filename} · ${formatBytes(item.file_size_bytes)}`;
  tile.oncontextmenu=e=>{
    e.preventDefault();const d=libraryDialog(item.filename);
    d.append(document.createTextNode(`Size: ${formatBytes(item.file_size_bytes)}`));
    if(window.__TAURI__&&item.downloaded!==0) {
      for(const [label,action] of [['Open','open'],['Show in file manager','reveal'],['Copy Path','path']])d.append(libraryButton(label,async()=>{
        const path=await window.__TAURI__.core.invoke('media_action',{id:item.id,action});
        if(action==='path')await navigator.clipboard.writeText(path); d.close();
      }));
    }
    if(item.origin_url)d.append(libraryButton('Copy Source URL',()=>navigator.clipboard.writeText(item.origin_url)));
    d.append(libraryButton('Preview, Rate and Tag',()=>{d.close();openLightbox(index);}));
  };return tile;
};
const filter=document.createElement('select');filter.className='btn btn-ghost';filter.id='size-filter';filter.setAttribute('aria-label','File size');
for(const [label,value] of [['All sizes',''],['Over 10 MB','10485760'],['Over 100 MB','104857600'],['Over 1 GB','1073741824'],['Unknown size','unknown']])filter.add(new Option(label,value));
document.querySelector('#sort-select').after(filter);filter.onchange=()=>loadView();
const gridMode=libraryButton('Grid / List',()=>{document.querySelector('#grid').classList.toggle('library-list');localStorage.setItem('library-view-mode',document.querySelector('#grid').classList.contains('library-list')?'list':'grid');});
filter.after(gridMode);if(localStorage.getItem('library-view-mode')==='list')document.querySelector('#grid').classList.add('library-list');
