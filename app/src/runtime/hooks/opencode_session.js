// Profile-local plugin. Only this terminal's native OpenCode session is reported.
import {writeFile,rename} from "node:fs/promises";
export const BasaltSession = async ({directory}) => {
  const path=process.env.BASALT_AGENT_STATE_FILE;
  let session=null;
  return {event:async ({event})=>{
    if (!path) return;
    const info=event.properties?.info;
    if (info?.parentID) return;
    if (info?.directory===directory && info.id) session=info.id;
    const id=event.properties?.sessionID || session;
    if (!id || id!==session) return;
    let state;
    if(event.type==="session.status") {const status=event.properties?.status?.type;if(status==="busy")state="working";else if(status==="idle")state="completed";else return;}
    else if(event.type==="session.idle")state="completed";
    else if(event.type==="permission.asked")state="waiting";
    else if(event.type==="permission.replied")state="working";
    else if(event.type==="session.error")state="error";
    else if(event.type==="session.created")state="idle";
    else return;
    const temp=path+".opencode.tmp";
    try {await writeFile(temp,JSON.stringify({provider:"opencode",state,session_id:id,turn_id:"",event:event.type}));await rename(temp,path);}catch{}
  }};
};
