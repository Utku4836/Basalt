// Opt-in verification CLI. No model calls, network or user configuration.
const cp = require('child_process');
const readline = require('readline');
const hook = process.argv[2];
const send = event => {
  const result = cp.spawnSync(hook, ['--agent-hook','codex'], {
    input: JSON.stringify({hook_event_name:event,session_id:'verification-session',turn_id:'verification-turn'}), encoding:'utf8',windowsHide:true
  });
  if (result.status !== 0) process.exit(2);
};
send('SessionStart');
readline.createInterface({input:process.stdin}).on('line', line => {
  if (line.trim()==='working') send('UserPromptSubmit');
  if (line.trim()==='stop') send('Stop');
  if (line.trim()==='exit') {send('SessionEnd');process.exit(0);}
});
