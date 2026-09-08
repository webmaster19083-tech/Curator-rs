const {test}=require('node:test');
const assert=require('node:assert/strict');
const vm=require('node:vm');
const fs=require('node:fs');
function fixture(item){
  let scheduled;const listeners={};
  const video={duration:200,currentTime:0,paused:true,playbackRate:1,
    addEventListener(name,fn){(listeners[name]??=[]).push(fn);},
    removeEventListener(name,fn){listeners[name]=(listeners[name]||[]).filter(f=>f!==fn);},
    dispatchEvent(event){for(const fn of listeners[event.type]||[])fn();},
    pause(){this.paused=true;this.dispatchEvent({type:'pause'});}};
  const ctx=vm.createContext({video,item,Event:class{constructor(type){this.type=type;}},setTimeout(fn,ms){scheduled={fn,ms};return 1;},clearTimeout(){scheduled=null;}});
  vm.runInContext(fs.readFileSync('static/virtual-clips.js','utf8'),ctx);
  vm.runInContext('bindVirtualClip(video,item)',ctx);
  return {video,emit:type=>video.dispatchEvent({type}),run:s=>vm.runInContext(s,ctx),timer:()=>scheduled};
}
test('virtual clip starts at saved offset, clamps seeks, and ends once',()=>{
  const f=fixture({clip_start_secs:60,clip_end_secs:90});let ended=0;
  f.video.addEventListener('ended',()=>ended++);f.emit('loadedmetadata');assert.equal(f.video.currentTime,60);
  f.video.paused=false;f.emit('play');assert.equal(f.timer().ms,30000);
  f.video.currentTime=75;assert.equal(f.run('clipProgress(video)'),.5);
  f.video.currentTime=2;f.emit('seeking');assert.equal(f.video.currentTime,60);
  f.video.currentTime=92;f.emit('timeupdate');assert.equal(f.video.currentTime,90);assert.equal(f.video.paused,true);assert.equal(ended,1);
  f.emit('timeupdate');assert.equal(ended,1);
  f.video.paused=false;f.emit('play');assert.equal(f.video.currentTime,60);assert.equal(f.video._clipEnded,false);
});
test('waiting and pause cancel boundary timer; speed and last short clip use correct time',()=>{
  const f=fixture({clip_start_secs:90,clip_end_secs:95});f.emit('loadedmetadata');f.video.paused=false;f.video.playbackRate=2;f.emit('play');assert.equal(f.timer().ms,2500);
  f.emit('waiting');assert.equal(f.timer(),null);f.emit('playing');assert.equal(f.timer().ms,2500);f.video.pause();assert.equal(f.timer(),null);
});
test('physical videos and old physical clips keep their full playback duration',()=>{
  const f=fixture({clip_parent_id:1,duration_secs:30});assert.equal(f.video._clipRange,undefined);assert.equal(f.run('clipDuration(video)'),200);
});
test('reused video detaches old range and loops within the current range',()=>{
  const f=fixture({clip_start_secs:60,clip_end_secs:90});f.video.loop=true;f.emit('loadedmetadata');f.video.paused=false;f.video.currentTime=91;f.emit('timeupdate');assert.equal(f.video.currentTime,60);assert.equal(f.video.paused,false);
  f.run('bindVirtualClip(video,{})');assert.equal(f.video._clipRange,undefined);f.video.currentTime=150;f.emit('timeupdate');assert.equal(f.video.currentTime,150);
});
