const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
function fixture(wakeLock) {
  const scroll = {children:[],scrollTop:0};
  const nodes = {'#feed-scroll':scroll,'#feed':{hidden:false},'#feed-status':{}};
  const ctx = vm.createContext({console,Set,Map,Math,Promise,setTimeout,clearTimeout,
    document:{visibilityState:'visible'},navigator:{wakeLock},el:id=>nodes[id]});
  const source = fs.readFileSync('static/app.js','utf8');
  vm.runInContext(source.slice(source.indexOf('const FEED_TARGET_BUFFER'), source.indexOf('// VR ')),ctx);
  const run = code=>vm.runInContext(code,ctx);
  run('feed.active=true; feed.page={more:false,pending:false};');
  const feed = run('feed');
  function section(id) {
    const media = {tagName:'VIDEO',pause(){this.paused=true;},removeAttribute(){this.cleared=true;},load(){this.unloaded=true;}};
    const node = {_item:{id},_mediaEl:media,_queued:true,offsetHeight:100,
      remove(){scroll.children.splice(scroll.children.indexOf(this),1);}};
    scroll.children.push(node); feed.queuedIds.add(id);
    return node;
  }
  return {run,feed,scroll,section,nodes};
}
test('pagination and in-flight loads must finish before recycling; reservations prevent duplicate selection',()=>{
  const {run,feed} = fixture();
  feed.items=[{id:1},{id:1},{id:2}];
  assert.equal(run('feedCandidate().id'),1);
  feed.queuedIds.add(1);
  assert.equal(run('feedCandidate().id'),2);
  feed.recyclePool.set(3,{id:3});feed.page.more=true;
  assert.equal(run('feedCandidate()'),null);assert.equal(feed.recycleMode,false);
  feed.page.more=false;feed.page.pending=true;
  assert.equal(run('feedCandidate()'),null);
  feed.page.pending=false;feed.inFlight=1;
  assert.equal(run('feedCandidate()'),null);
  feed.inFlight=0;
  assert.equal(run('feedCandidate().id'),3);assert.equal(feed.recycleMode,true);
  feed.queuedIds.add(3);assert.equal(run('feedCandidate()'),null);
});
test('recycling avoids current and recent items with a fallback for small libraries',()=>{
  const {run,feed} = fixture();
  for(let id=1;id<=12;id++)feed.recyclePool.set(id,{id});
  feed.activeSection={_item:{id:12}};feed.recentIds=[1,2,3,4,5,6,7,8,9,10];
  assert.equal(run('feedCandidate().id'),11);
  feed.queuedIds.add(11);
  assert.notEqual(run('feedCandidate().id'),12);
  feed.review=true;assert.equal(run('feedCandidate()'),null);
});
test('long navigation evicts old resources and retains at most two previous, current, three ahead',()=>{
  const {run,feed,scroll,section} = fixture();
  let current=section(1);feed.activeSection=current;
  for(let id=2;id<=1000;id++) {
    current=section(id);feed.activeSection=current;
    run('feedEvict()');assert.ok(scroll.children.length<=3);
  }
  const old=scroll.children[0];
  feed.activeSection=section(1001);run('feedEvict()');
  assert.equal(old._mediaEl.paused,true);assert.equal(old._mediaEl.cleared,true);assert.equal(old._mediaEl.unloaded,true);
  for(let id=1002;id<=1010;id++)section(id);
  run('feedEvict()');assert.equal(scroll.children.length,6);
});
test('exit cancels pending media and releases wake lock and history',async()=>{
  let releases=0;
  const f=fixture({request:async()=>({release:async()=>{releases++;},addEventListener(){}})});
  await f.run('feedAcquireWakeLock()');assert.ok(f.feed.wakeLock);
  const node=f.section(1);f.feed.activeSection=node;
  let cancelled=false;node._mediaEl._cancelLoad=()=>{cancelled=true;};
  f.run('exitFeed()');
  assert.equal(cancelled,true);assert.equal(releases,1);assert.equal(f.feed.wakeLock,null);
  assert.equal(f.scroll.children.length,0);assert.equal(f.feed.queuedIds.size,0);assert.equal(f.feed.active,false);
});
test('late wake request after exit is released and unsupported/denied API is harmless',async()=>{
  let resolve,releases=0;
  const f=fixture({request:()=>new Promise(r=>{resolve=r;})});
  const pending=f.run('feedAcquireWakeLock()');f.run('exitFeed()');
  resolve({release:async()=>{releases++;}});await pending;
  assert.equal(releases,1);assert.equal(f.feed.wakeLock,null);
  await fixture().run('feedAcquireWakeLock()');
  await fixture({request:async()=>{throw Error('denied');}}).run('feedAcquireWakeLock()');
});

test('review uses five stars; left swipe animates without rating, right swipe approves',async()=>{
  const f=fixture();
  f.run(`
    var calls=[], animations=[], advanced=0;
    var window={matchMedia:()=>({matches:false})};
    var state={currentItems:[]}, toast=()=>{};
    var api=async(url,body)=>{calls.push({url,body});return {rating:4,auto_rating:4,rating_reviewed:true};};
    feedGoNext=()=>{advanced++;};
    class Node {
      constructor(tag){this.tagName=tag;this.children=[];this.listeners={};this.style={};this.attrs={};this.classList={add(){},remove(){},toggle(){}};}
      appendChild(n){this.children.push(n);return n;}
      setAttribute(k,v){this.attrs[k]=v;}
      addEventListener(k,v){this.listeners[k]=v;}
      querySelectorAll(tag){return this.children.flatMap(c=>[...(c.tagName===tag?[c]:[]),...c.querySelectorAll(tag)]);}
      focus(){this.focused=true;}
      animate(frames){animations.push(frames);return {finished:Promise.resolve()};}
      setPointerCapture(){} hasPointerCapture(){return false;}
    }
    document.createElement=tag=>new Node(tag);
    var section=new Node('section');
    feed.activeSection=section;
    feedBuildReviewControls(section,{id:1,auto_rating:4,rating:4});
    var stars=section.querySelectorAll('button').filter(b=>b.attrs['aria-label']);
  `);
  assert.equal(f.run('stars.length'),5);
  assert.equal(f.run('stars[2].textContent'),'★');
  assert.equal(f.run("stars[2].attrs['aria-label']"),'Rate 3 stars');
  f.run(`section.listeners.pointerdown({isPrimary:true,clientX:150,clientY:0,target:{closest:()=>null}});
    section.listeners.pointermove({pointerId:1,clientX:60,clientY:0});`);
  assert.match(f.run('section.children[0].style.transform'),/translateX\(-90px\)/);
  f.run('section.listeners.pointerup({pointerId:1,clientX:60,clientY:0});');
  assert.equal(f.run('calls.length'),0);assert.equal(f.run('stars[0].focused'),true);
  assert.equal(f.run('animations.length'),1);
  f.run(`section.listeners.pointerdown({isPrimary:true,clientX:0,clientY:0,target:{closest:()=>null}});
    section.listeners.pointerup({pointerId:1,clientX:100,clientY:0});`);
  await new Promise(r=>setImmediate(r));
  assert.match(f.run('calls[0].url'),/rating\/approve$/);
  assert.equal(f.run('advanced'),1);
  assert.equal(f.run('animations.length'),2);
});
