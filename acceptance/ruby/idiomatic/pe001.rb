puts (1...1000).select { it % 3 == 0 || it % 5 == 0 }.sum
