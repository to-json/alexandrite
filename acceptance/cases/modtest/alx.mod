# main module
module example.com/app

require (
  example.com/lib v1.0.0
  example.com/dep2 v1.0.0  # lib wants v1.2.0; MVS picks that
)

replace example.com/lib => ./deps/lib
