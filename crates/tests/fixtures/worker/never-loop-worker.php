<?php
// this entrypoint never pulls a unit, so it must classify as a boot failure, not a servable worker.
\Rapira\log('booted');
