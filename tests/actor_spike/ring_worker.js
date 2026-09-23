// Echo worker over the SAB ring pair — the Actor host shape: block on the
// request ring, process, respond on the response ring.
import { Ring } from './ring.js';

onmessage = (e) => {
  const { sab, capacity, reqOffset, respOffset } = e.data;
  const req = new Ring(sab, reqOffset, capacity);
  const resp = new Ring(sab, respOffset, capacity);
  postMessage('ready');
  for (;;) {
    const frame = req.readFrameBlocking();
    if (frame.length === 1 && frame[0] === 0xff) return; // shutdown
    resp.writeFrameBlocking(frame);
  }
};
