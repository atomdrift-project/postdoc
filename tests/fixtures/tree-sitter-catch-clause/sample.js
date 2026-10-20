try { run(); } catch (caught) { result = caught.handler.run(); }
try { run(); } catch (caught) { result = unrelated.handler.run(); }
