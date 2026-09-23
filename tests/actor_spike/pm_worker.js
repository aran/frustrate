// Echo worker over postMessage — the plain-postMessage baseline. Echoes the buffer
// back; transfers it when the request was transferred.
onmessage = (e) => {
  if (e.data === 'init') {
    postMessage('ready');
    return;
  }
  const { buf, transfer } = e.data;
  if (transfer) {
    postMessage({ buf }, [buf]);
  } else {
    postMessage({ buf });
  }
};
