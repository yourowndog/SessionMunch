// Focused runtime regression for the generated V2 binding; no hub or model calls.
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const directory=await mkdtemp(join(tmpdir(),'sessionmunch-opencode2-'));
try {
  const binding=await readFile(new URL('../../crates/sessionmunch-cli/templates/plugins/opencode2_binding.ts',import.meta.url),'utf8');
  const prelude=`
  const handoffFetches=new Map(); const startedSessions=new Set();
  let fetched=0; let drained=0; const checkpoints=[];
  const cwdFor=(_id,directory)=>directory;
  const startSession=(id)=>{if(id)startedSessions.add(id)};
  const endSession=(id)=>startedSessions.delete(id);
  const postHook=()=>{}; const textFromParts=()=>'';
  const fetchHandoff=async()=>{fetched++;return 'stable handoff'};
  const postPreCompact=(id)=>checkpoints.push(id);
  let drainGate;
  const drainHookQueueForDispose=async()=>{drained++;if(drainGate)await drainGate};
  export const setDrainGate=(gate)=>{drainGate=gate};
  export const state=()=>({fetched,drained,checkpoints});
  `;
  const file=join(directory,'binding.ts');await writeFile(file,prelude+binding);
  const {default:plugin,state,setDrainGate}=await import(pathToFileURL(file));
  const callbacks={};let disposed=0;
  const hook=async(name,fn)=>{callbacks[name]=fn;return {dispose:async()=>{disposed++}}};
  const cleanup=await plugin.setup({location:{directory},event:{subscribe:()=>({async *[Symbol.asyncIterator](){}})},session:{hook},tool:{hook}});
  const first={sessionID:'fixture',system:[]};await Promise.all([callbacks.context(first),callbacks.context(first)]);
  const second={sessionID:'fixture',system:[]};await callbacks.context(second);
  await callbacks.context(second);
  assert.deepEqual(first.system,[{type:'text',text:'stable handoff'}]);
  assert.deepEqual(second.system,first.system);
  assert.equal(state().fetched,1,'fetch/claim once per session');
  const other={sessionID:'other',system:[]};await callbacks.context(other);
  assert.equal(state().fetched,2,'separate sessions do not share a fetch');
  await callbacks.context({sessionID:'fixture'});
  assert.equal(state().fetched,2,'invalid context is ignored');
  let releaseDrain;
  setDrainGate(new Promise(resolve=>{releaseDrain=resolve}));
  let finished=false;
  const compacting=callbacks.compaction({sessionID:'fixture'}).then(()=>{finished=true});
  await Promise.resolve();
  assert.deepEqual(state().checkpoints,['fixture']);
  assert.equal(state().drained,1);
  assert.equal(finished,false,'native compaction waits for the bounded drain');
  releaseDrain();await compacting;setDrainGate(undefined);
  assert.equal(finished,true);
  await cleanup();assert.equal(disposed,5);
  // A supported native hook suppresses lifecycle-event duplicates.
  let finishStream;
  const streamDone=new Promise(resolve=>{finishStream=resolve});
  const nativeCleanup=await plugin.setup({location:{directory},event:{subscribe:()=>({async *[Symbol.asyncIterator](){
    yield {type:'session.compaction.started',data:{sessionID:'duplicate'}};
    yield {type:'session.compaction.ended',data:{sessionID:'duplicate'}};
    finishStream();
  }})},session:{hook},tool:{hook}});
  await streamDone;
  assert.deepEqual(state().checkpoints,['fixture'],'native events do not double-capture');
  await nativeCleanup();assert.equal(disposed,10);
  // Older hosts that reject registration still capture via lifecycle events.
  let finishFallback;
  const fallbackDone=new Promise(resolve=>{finishFallback=resolve});
  const oldHook=async(name,fn)=>{if(name==='compaction')throw new Error('unsupported');return hook(name,fn)};
  const fallbackCleanup=await plugin.setup({location:{directory},event:{subscribe:()=>({async *[Symbol.asyncIterator](){
    yield {type:'session.compaction.started',data:{sessionID:'fallback'}};
    finishFallback();
  }})},session:{hook:oldHook},tool:{hook}});
  await fallbackDone;
  assert.deepEqual(state().checkpoints,['fixture','fallback']);
  await fallbackCleanup();assert.equal(disposed,14);
  console.log('OpenCode2 stable recall, per-context dedup, native compaction, fallback and cleanup PASS');
} finally {await rm(directory,{recursive:true,force:true})}
