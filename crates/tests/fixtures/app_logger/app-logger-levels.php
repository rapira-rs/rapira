<?php

// The call without a level checks that C applies the `= LogLevel::Info` default of the stub, because that default is only reflection metadata.
\Rapira\log('lvl-error', \Rapira\LogLevel::Error);
\Rapira\log('lvl-warning', \Rapira\LogLevel::Warning);
\Rapira\log('lvl-info', \Rapira\LogLevel::Info);
\Rapira\log('lvl-debug', \Rapira\LogLevel::Debug);
\Rapira\log('lvl-trace', \Rapira\LogLevel::Trace);
\Rapira\log('lvl-omitted');

echo 'logged';
