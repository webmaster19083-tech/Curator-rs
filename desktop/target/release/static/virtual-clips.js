'use strict';
function clipStart(video) { return video._clipRange ? video._clipRange.start : 0; }
function clipDuration(video) { return video._clipRange ? video._clipRange.end-video._clipRange.start : video.duration; }
function clipProgress(video) { return Math.max(0,Math.min(1,(video.currentTime-clipStart(video))/clipDuration(video))); }
function bindVirtualClip(video,item) {
  if(video._clipItem === item)return;
  if(video._clipCleanup)video._clipCleanup();
  video._clipItem=item;
  if (item.clip_start_secs == null) return;
  const start=Number(item.clip_start_secs), end=Number(item.clip_end_secs);
  if (!Number.isFinite(start)||!Number.isFinite(end)||start<0||end<=start) return;
  video._clipRange={start,end}; video._clipEnded=false;
  let timer;
  const listeners=[];
  const on=(name,fn)=>{listeners.push([name,fn]);video.addEventListener(name,fn);};
  video._clipCleanup=()=>{clearTimeout(timer);for(const [name,fn] of listeners)video.removeEventListener(name,fn);delete video._clipRange;video._clipEnded=false;delete video._clipCleanup;};
  const finish=()=>{
    if(video._clipEnded)return;
    if(video.loop){video.currentTime=start;schedule();return;}
    video._clipEnded=true;
    clearTimeout(timer);
    video.pause();
    video.currentTime=end;
    video.dispatchEvent(new Event('ended'));
  };
  const schedule=()=>{
    clearTimeout(timer);
    if(!video.paused&&!video._clipEnded)timer=setTimeout(finish,Math.max(0,(end-video.currentTime)/Math.max(.01,video.playbackRate)*1000));
  };
  on('loadedmetadata',()=>{video.currentTime=start;});
  on('play',()=>{
    if(video._clipEnded||video.currentTime<start||video.currentTime>=end){video._clipEnded=false;video.currentTime=start;}
    schedule();
  });
  on('seeking',()=>{
    clearTimeout(timer);
    if(video._clipEnded)return;
    if(video.currentTime<start)video.currentTime=start;
    if(video.currentTime>=end){finish();return;}
  });
  on('seeked',schedule);
  on('timeupdate',()=>{if(!video.paused&&video.currentTime>=end)finish();else schedule();});
  on('ratechange',schedule);
  on('playing',schedule);
  on('pause',()=>clearTimeout(timer));
  on('waiting',()=>clearTimeout(timer));
  on('emptied',()=>clearTimeout(timer));
}
