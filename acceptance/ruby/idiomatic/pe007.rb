require_relative "lib/primes"
puts primes.lazy.drop(10_000).first
