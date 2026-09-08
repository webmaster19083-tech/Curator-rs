const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

// Exercise the browser's real pagination functions with a small DOM/API fixture.
function browser(api) {
  const source = fs.readFileSync('static/app.js', 'utf8');
  const start = source.indexOf('let viewRequestSeq = 0;');
  const end = source.indexOf('// Setting a <video>', start);
  const nodes = new Map();
  const state = {view:{type:'all'},typeFilter:'all',sortOrder:'default',maxRatingFilter:'',PAGE_SIZE:150,currentItems:[],page:0};
  const context = vm.createContext({state,api,URLSearchParams,Set,ss:{active:false},toast:()=>{},buildTile:item=>item,
    document:{querySelector:()=>null,createDocumentFragment:()=>({items:[],appendChild(item){this.items.push(item);}})},
    el:selector=>{
      if(!nodes.has(selector)) nodes.set(selector,{hidden:false,items:[],set innerHTML(_){this.items=[];},appendChild(fragment){this.items.push(...fragment.items);}});
      return nodes.get(selector);
    }});
  vm.runInContext(source.slice(start,end),context);
  return {context,state,nodes,run:code=>vm.runInContext(code,context)};
}

test('browse fetches only one initial page and concurrent scrolls append the next page once',async()=>{
  const calls=[];
  const app=browser(async url=>{
    calls.push(url);
    if(url.includes('cursor=')) {await new Promise(r=>setTimeout(r,5));return {media:Array.from({length:40},(_,i)=>({id:i+151})),has_more:false,next_cursor:null};}
    return {media:Array.from({length:150},(_,i)=>({id:i+1})),has_more:true,next_cursor:'cursor-one'};
  });
  await app.run('loadView()');
  assert.equal(calls.length,1);assert.equal(app.state.currentItems.length,150);
  await Promise.all([app.run('renderNextPage()'),app.run('renderNextPage()')]);
  assert.equal(calls.length,2);assert.equal(app.nodes.get('#grid').items.length,190);
  assert.equal(new Set(app.nodes.get('#grid').items.map(i=>i.id)).size,190);
  assert.match(calls[1],/cursor=cursor-one/);
});

test('a response from an old view cannot overwrite a newer view',async()=>{
  let release;
  const app=browser(url=>url.includes('source_id=1') ? new Promise(r=>{release=r;}) : Promise.resolve({media:[{id:2}],has_more:false}));
  app.state.view={type:'creator',id:1};const old=app.run('loadView()');
  app.state.view={type:'creator',id:2};await app.run('loadView()');
  release({media:[{id:1}],has_more:false});await old;
  assert.equal(app.state.currentItems[0].id,2);
});

test('type, rating and tags are sent to the server before pagination',async()=>{
  let received;
  const app=browser(async url=>{received=new URL(url,'http://localhost');return {media:[],has_more:false};});
  app.state.ratingStatus='needs_review';app.state.typeFilter='clip';app.state.tagFilter='two words';app.state.maxRatingFilter='3';
  await app.run('loadView()');
  assert.equal(received.searchParams.get('rating_status'),'needs_review');
  assert.equal(received.searchParams.get('media_type'),'clip');
  assert.equal(received.searchParams.get('tag'),'two words');
  assert.equal(received.searchParams.get('max_rating'),'3');
});
